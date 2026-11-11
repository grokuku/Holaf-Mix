#!/usr/bin/env bash
# ============================================================================
#  scripts/start-pipewire-headless.sh — Run a throwaway PipeWire daemon
#  (pipewire + wireplumber + a private dbus session) without a desktop session.
# ============================================================================
#
#  Purpose
#  -------
#  The Rust `holaf-pw` backend and its live tests need a *running* PipeWire
#  daemon.  A bare container (no systemd, no $XDG_RUNTIME_DIR, no session bus)
#  has none, and `wireplumber` refuses to start without a session bus.
#  This script recreates the exact recipe validated during the J1 spike
#  (see docs/j1-spike-recon.md §A "Lancement headless reproductible"):
#
#    1. a private XDG_RUNTIME_DIR (0700), default /tmp/pw-runtime-holaf
#    2. a private `dbus-daemon --session` on $XDG_RUNTIME_DIR/bus
#    3. `pipewire` then `wireplumber`, detached (setsid), logs in $RUNTIME_DIR
#    4. a readiness poll via `pw-dump` (5 s timeout by default)
#
#  Usage
#  -----
#      ./scripts/start-pipewire-headless.sh              # start (idempotent)
#      ./scripts/start-pipewire-headless.sh start        # same
#      ./scripts/start-pipewire-headless.sh status       # pids + health
#      ./scripts/start-pipewire-headless.sh stop         # clean shutdown
#      ./scripts/start-pipewire-headless.sh restart
#      eval "$(./scripts/start-pipewire-headless.sh --export)"   # env vars only
#
#  Environment
#  -----------
#      HOLAF_PW_RUNTIME_DIR   runtime dir (default /tmp/pw-runtime-holaf)
#      HOLAF_PW_READY_TIMEOUT readiness timeout in seconds (default 5)
#
#  Exit status: 0 = daemon ready / stop succeeded, non-zero otherwise.
#
#  Note: checks ignore zombie processes (a `Z` state fools `pgrep`/`kill -0`):
#  a defunct child can linger in the container and look alive.
# ============================================================================

set -euo pipefail

RUNTIME_DIR="${HOLAF_PW_RUNTIME_DIR:-/tmp/pw-runtime-holaf}"
READY_TIMEOUT="${HOLAF_PW_READY_TIMEOUT:-5}"
PID_PW="$RUNTIME_DIR/pipewire.pid"
PID_WP="$RUNTIME_DIR/wireplumber.pid"
PID_DBUS="$RUNTIME_DIR/dbus.pid"
LOG_PW="$RUNTIME_DIR/pipewire.log"
LOG_WP="$RUNTIME_DIR/wireplumber.log"
LOG_DBUS="$RUNTIME_DIR/dbus.log"
BUS_ADDRESS="unix:path=$RUNTIME_DIR/bus"

log()  { printf '\033[1;34m[pw-headless]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[pw-headless]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[pw-headless]\033[0m %s\n' "$*" >&2; }

# --- Helpers ----------------------------------------------------------------

# Print the environment needed by clients (pw-cli, holafd, tests).
print_exports() {
    printf 'export XDG_RUNTIME_DIR=%q\n'                  "$RUNTIME_DIR"
    printf 'export PIPEWIRE_RUNTIME_DIR=%q\n'            "$RUNTIME_DIR"
    printf 'export DBUS_SESSION_BUS_ADDRESS=%q\n'        "$BUS_ADDRESS"
}

require_tools() {
    local missing=()
    for tool in pipewire wireplumber pw-dump dbus-daemon setsid; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if (( ${#missing[@]} > 0 )); then
        err "missing tool(s): ${missing[*]}"
        err "install: apt-get install -y pipewire pipewire-bin wireplumber dbus"
        exit 1
    fi
}

# True if $1 is a live, non-zombie process.
pid_alive() {
    local pid="${1:-}"
    [[ -n "$pid" ]] || return 1
    kill -0 "$pid" 2>/dev/null || return 1
    local state
    state="$(ps -o stat= -p "$pid" 2>/dev/null | tr -d '[:space:]')"
    # Empty state => process vanished between kill(0) and ps.
    [[ -n "$state" && "${state:0:1}" != "Z" ]]
}

# True if the daemon recorded in pidfile $1 is alive.
daemon_alive() {
    local pidfile="$1" pid
    [[ -f "$pidfile" ]] || return 1
    pid="$(cat "$pidfile" 2>/dev/null || true)"
    pid_alive "$pid"
}

first_alive_pid() {
    local pidfile
    for pidfile in "$PID_PW" "$PID_WP" "$PID_DBUS"; do
        if daemon_alive "$pidfile"; then
            cat "$pidfile"
            return 0
        fi
    done
    return 1
}

# Launch a daemon detached in its own session; write its real pid to $pidfile.
# The `bash -c` wrapper makes $$ exact even if `setsid` has to fork.
launch() {
    local pidfile="$1" logfile="$2"
    shift 2
    setsid bash -c '
        pidfile="$1"; logfile="$2"; shift 2
        echo $$ > "$pidfile"
        exec "$@" >"$logfile" 2>&1 </dev/null
    ' _ "$pidfile" "$logfile" "$@" </dev/null &
}

stop_pidfile() {
    local name="$1" pidfile="$2" pid
    [[ -f "$pidfile" ]] || { log "$name: not running"; return 0; }
    pid="$(cat "$pidfile" 2>/dev/null || true)"
    if ! pid_alive "$pid"; then
        rm -f "$pidfile"
        log "$name: stale pidfile removed"
        return 0
    fi
    log "$name: stopping pid $pid"
    kill -TERM "$pid" 2>/dev/null || true
    local i
    for ((i = 0; i < 30; i++)); do
        pid_alive "$pid" || break
        sleep 0.1
    done
    if pid_alive "$pid"; then
        warn "$name: SIGTERM ignored, sending SIGKILL"
        kill -KILL "$pid" 2>/dev/null || true
        sleep 0.2
    fi
    rm -f "$pidfile"
}

# Poll until the daemon answers (`pw-dump` exits 0), or timeout.
wait_ready() {
    local deadline=$((SECONDS + READY_TIMEOUT))
    while (( SECONDS < deadline )); do
        if daemon_alive "$PID_PW" && timeout 2 pw-dump >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

dump_log_tail() {
    local logfile="$1"
    [[ -f "$logfile" ]] || return 0
    warn "--- tail $logfile ---"
    tail -n 20 "$logfile" >&2 || true
    warn "---------------------"
}

# --- Commands ---------------------------------------------------------------

cmd_start() {
    require_tools
    mkdir -p "$RUNTIME_DIR"
    chmod 700 "$RUNTIME_DIR"

    if daemon_alive "$PID_PW" && timeout 2 pw-dump >/dev/null 2>&1; then
        log "PipeWire already running (pid $(cat "$PID_PW"))"
        cmd_status
        return 0
    fi

    # A stale pidfile must not hide a fresh start.
    daemon_alive "$PID_PW" || rm -f "$PID_PW"

    export XDG_RUNTIME_DIR="$RUNTIME_DIR"
    export PIPEWIRE_RUNTIME_DIR="$RUNTIME_DIR"

    # 1. Private session bus (wireplumber requires one).
    if ! daemon_alive "$PID_DBUS"; then
        rm -f "$PID_DBUS" "$RUNTIME_DIR/bus" "$RUNTIME_DIR/bus.pid"
        export DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS"
        log "Starting dbus-daemon on $BUS_ADDRESS"
        dbus-daemon --session --fork --print-pid --nopidfile \
            --address="$BUS_ADDRESS" 2>"$LOG_DBUS" >"$PID_DBUS"
        if ! daemon_alive "$PID_DBUS"; then
            err "dbus-daemon failed to start"
            dump_log_tail "$LOG_DBUS"
            return 1
        fi
    fi
    export DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS"

    # 2. PipeWire.
    if ! daemon_alive "$PID_PW"; then
        log "Starting pipewire (log: $LOG_PW)"
        : > "$LOG_PW"
        launch "$PID_PW" "$LOG_PW" pipewire
        if ! wait_ready; then
            err "PipeWire did not become ready within ${READY_TIMEOUT}s"
            dump_log_tail "$LOG_PW"
            stop_pidfile "pipewire" "$PID_PW"
            return 1
        fi
    fi

    # 3. WirePlumber (session manager: metadata, default nodes).
    if ! daemon_alive "$PID_WP"; then
        log "Starting wireplumber (log: $LOG_WP)"
        : > "$LOG_WP"
        launch "$PID_WP" "$LOG_WP" wireplumber
        # WirePlumber is optional for the tests but expected to settle quickly;
        # do not fail the whole start if it is slow -- report it instead.
        local i
        for ((i = 0; i < 25; i++)); do
            daemon_alive "$PID_WP" && break
            sleep 0.1
        done
        daemon_alive "$PID_WP" || warn "wireplumber exited early (see $LOG_WP)"
    fi

    log "PipeWire ready"
    cmd_status
    echo
    log "In this shell run:"
    print_exports
    return 0
}

cmd_stop() {
    export XDG_RUNTIME_DIR="$RUNTIME_DIR"
    export PIPEWIRE_RUNTIME_DIR="$RUNTIME_DIR"
    export DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS"

    # Reverse dependency order: session manager, then server, then bus.
    stop_pidfile "wireplumber" "$PID_WP"
    stop_pidfile "pipewire"  "$PID_PW"
    stop_pidfile "dbus-daemon" "$PID_DBUS"

    if first_alive_pid >/dev/null 2>&1; then
        err "some daemons are still alive"
        return 1
    fi
    rm -f "$RUNTIME_DIR/bus" "$RUNTIME_DIR/bus.pid"
    log "All headless daemons stopped (runtime dir kept: $RUNTIME_DIR)"
    return 0
}

cmd_status() {
    # A standalone `status` must find the socket even without a prior export.
    export XDG_RUNTIME_DIR="$RUNTIME_DIR"
    export PIPEWIRE_RUNTIME_DIR="$RUNTIME_DIR"
    export DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS"

    local ok=0 pid
    for entry in "pipewire:$PID_PW" "wireplumber:$PID_WP" "dbus-daemon:$PID_DBUS"; do
        name="${entry%%:*}"; pidfile="${entry#*:}"
        if pid="$(cat "$pidfile" 2>/dev/null)" && pid_alive "$pid"; then
            printf '  %-12s running  pid %s\n' "$name" "$pid"
        else
            printf '  %-12s stopped\n' "$name"
            [[ "$name" == "pipewire" ]] && ok=1
        fi
    done
    if timeout 2 pw-dump >/dev/null 2>&1; then
        printf '  %-12s answering pw-dump\n' "pipewire"
    else
        printf '  %-12s NOT answering\n' "pipewire"
        ok=1
    fi
    printf '  runtime dir  %s\n' "$RUNTIME_DIR"
    return "$ok"
}

usage() {
    cat <<EOF
Usage: $(basename "$0") [command]

Commands:
  start (default)  start pipewire + wireplumber + private dbus session
  stop             stop everything started by this script
  restart          stop then start
  status           show pids and daemon health
  --export | env   print the shell exports to eval/source (no start)

Environment:
  HOLAF_PW_RUNTIME_DIR    runtime dir (default /tmp/pw-runtime-holaf)
  HOLAF_PW_READY_TIMEOUT  readiness timeout seconds (default 5)

Examples:
  ./scripts/start-pipewire-headless.sh
  eval "\$(./scripts/start-pipewire-headless.sh --export)"
  ./scripts/start-pipewire-headless.sh stop
EOF
}

case "${1:-start}" in
    start)            cmd_start ;;
    stop)             cmd_stop ;;
    restart)          cmd_stop && cmd_start ;;
    status)           cmd_status ;;
    --export|export|env) print_exports ;;
    -h|--help|help)   usage ;;
    *)
        err "unknown command: $1"
        usage >&2
        exit 2
        ;;
esac
