# L'hôte FX dédié (`holafd --fx-host`)

Document de référence du correctif « hachage audio (chopping) en mode Rust
quand une chaîne FX est active ».  Il décrit **pourquoi** la chaîne n'est plus
hébergée dans `holafd`, **comment** elle l'est désormais, et **comment le
vérifier** sur une machine à audio réel.

Code :

| Fichier | Rôle |
|---|---|
| `rust/crates/holaf-pw/src/fxhost/proto.rs` | protocole NDJSON (pur, testé unitairement) |
| `rust/crates/holaf-pw/src/fxhost/host.rs` | le processus enfant (`holafd --fx-host`) |
| `rust/crates/holaf-pw/src/fxhost/client.rs` | côté `holafd` : spawn, requête/réponse, mort |
| `rust/crates/holaf-pw/src/chainhost.rs` | `ChainHost` : *où* la chaîne est hébergée + respawn |
| `rust/crates/holaf-pw/src/engine.rs` | `ChainSpec` (SPA-JSON) + `push_chain_params` |
| `tools/audio_bench.py` | banc A/B (cas `ref`, `pwcli`, `holafd`, `fxhost`) |

---

## 1. Le problème

`libpipewire-module-filter-chain` s'exécute **dans le processus qui le
charge** : il crée deux *streams* PipeWire dans le contexte du client, et c'est
la boucle d'événements (`mainloop`) de ce client qui les rend programmables
(négociation de format, activation, `set_param`), les callbacks `process`
tournant ensuite sur le *data-loop* de ce même processus.

Le moteur Python n'a jamais haché parce qu'il charge la chaîne dans un
processus `pw-cli` dédié dont **le seul métier** est de faire tourner
`pw_main_loop_run()`.

Le moteur Rust l'hébergeait dans `holafd`, donc sur la même boucle que : le
socket IPC (NDJSON), la réconciliation du graphe, le tick de réparation, le
logging, le watchdog… Sur un device tolérant (null-sink, C-Media USB) ça passe ;
sur une carte USB à paquets de 1 ms (PreSonus Studio 26c) le moindre retard de
service s'entend comme un hachage on/off continu.

Diagnostic confirmé par l'utilisateur sur sa machine (`status` →
`"degraded": false`, `"pending": []`) : **le graphe est parfait**, ce n'est ni
un problème de construction, ni de device manquant, ni de priorité temps réel.

## 2. La nouvelle architecture

```
   Python UI ──NDJSON──► holafd ──────────────────────────────┐
                          │  (graphe, routage, volume, IPC)  │
                          │                                  │
                          │ NDJSON sur stdin/stdout          │
                          ▼                                  │
                    holafd --fx-host  (processus enfant)      │
                          │                                  │
                          └── pw_context ─────────────────────┤
                                 ├── module-filter-chain par strip
                                 └── pw_main_loop_run()  (bloquant, pur événementiel)
                                                                 ▼
                                                          démon PipeWire
```

`holafd` garde **tout** le graphe : null-sinks, liens, volume/mute, métadonnée
du sink par défaut, VU-mètres, routage.  Il délègue uniquement le chargement des
`module-filter-chain`, qui restent les nœuds `input.Holaf_FX_<uid>` /
`output.Holaf_FX_<uid>` du graphe — les liens ne changent donc pas.

### 2.1 Pourquoi le processus enfant est « comme pw-cli »

* une seule connexion PipeWire, config client par défaut (`ContextRc::new(…, None)`),
  donc **exactement** le même traitement temps réel que `pw-cli` (voir §5) ;
* `stdin` est enregistré comme **source d'IO du mainloop**
  (`loop_.add_io(…, IoFlags::IN, …)`) : une commande réveille la boucle par le
  même `poll()` que l'audio. Pas de thread lecteur, pas de timeout, pas de
  file d'ordres à drainer ;
* le reste du temps : `mainloop.run()` = `pw_main_loop_run()`, bloquant pur
  événementiel. Aucun autre travail sur ce thread.

### 2.2 Ce que fait `holafd` en moins

`ChainHost::pumps_own_mainloop()` est faux en mode externe : le worker de
`holafd` **bloque** sur son canal d'ordres (0 % de CPU) et se réveille toutes
les 500 ms pour un `waitpid`-léger (watchdog de l'hôte).  Mesuré sur le banc :

| mode | CPU `holafd` | CPU hôte FX | total |
|---|---|---|---|
| `inprocess` (avant) | **6,25 – 6,38 %** | — | 6,2 – 6,4 % |
| `external` (défaut) | **0,88 %** | 4,4 % | 5,2 – 5,8 % |

C'est le cœur du correctif : le thread qui sert la chaîne n'est plus partagé
avec l'IPC et la réconciliation.

## 3. Protocole `holafd` ↔ `--fx-host`

NDJSON, une requête = une ligne, une réponse = une ligne.  `id` est renvoyé
dans la réponse (les commandes sont envoyées une par une et attendues, mais le
protocole ne l'impose pas).

```text
→ {"id":1,"cmd":"load_chain","uid":"main","config":"{ node.name = … }","anchor_node_id":42}
← {"id":1,"ok":true,"chain":{"uid":"main","input_node":"input.Holaf_FX_main",
                             "output_node":"output.Holaf_FX_main"}}
→ {"id":2,"cmd":"unload_chain","uid":"main"}      ← {"id":2,"ok":true,"removed":true}
→ {"id":3,"cmd":"unload_all"}                     ← {"id":3,"ok":true,"unloaded":["main"]}
→ {"id":4,"cmd":"set_params","uid":"main","props":{"eq_L:100Hz gain":3.5,…}}
                                                  ← {"id":4,"ok":true}
→ {"id":5,"cmd":"ping"}                           ← {"id":5,"ok":true,"pong":true,"uptime_ms":…}
→ {"id":6,"cmd":"status"}                         ← {"id":6,"ok":true,"chains":["main"],"pid":…}
→ {"id":7,"cmd":"stop"}                           ← {"id":7,"ok":true}   (puis exit 0)
```

Erreur : `{"id":N,"ok":false,"error":{"code":"…","message":"…"}}`.

* `config` est le SPA-JSON **exact** que `holafd` construirait
  (`engine::ChainSpec::for_strip`, aussi exposé par le binaire de diagnostic
  `holaf-fxcfg`) : l'hôte ne re-dérive jamais la config, il l'exécute.
* `anchor_node_id` est informatif (le `target.object` est déjà dans la config) ;
  l'hôte logge en debug si l'ancre n'est pas encore dans sa vue du registre.
* `bad_request` : une ligne illisible reçoit **toujours** une réponse, l'`id`
  étant récupéré au mieux (`"id":N`), pour que le parent ne se bloque jamais.
* Les codes d'erreur renvoyés à l'utilisateur restent un **espace fermé** :
  l'hôte ne peut pas inventer un code inconnu du client (`host_code()` mappe
  l'inconnu sur `fx_host_error`).

`set_params` est implémenté côté hôte (bind du nœud `input.Holaf_FX_<uid>` dans
sa propre vue du registre, sans round-trip).  `holafd` continue néanmoins
d'écrire les `Props` par **sa** connexion (`push_chain_params`), qui est
agnostique de l'hébergeur et déjà couverte par les tests : deux chemins
valides, un seul utilisé en production.

## 4. Cycle de vie, mort et respawn

```
holafd démarre
  └─ ChainHost::from_mode(External) ─► spawn() ─► ping (handshake 3 s)
        échec ─► log ERROR + repli in-process (le graphe marche quand même)
1re chaîne
  └─ host.load(uid, config) ─► "desired" enregistré ─► load_chain
hôte mort (crash, OOM, kill)
  ├─ détection : ensure_alive() avant chaque opération + watchdog 500 ms
  ├─ respawn + rejeu de TOUTES les chaînes désirées (source de vérité = parent)
  ├─ take_restored() ─► PendingIssue(Retry::Strip) ─► la passe de réparation
  │   refait le lien intra-strip et le volume (les nœuds de la chaîne sont neufs)
  └─ status.fx_host.respawns += 1
arrêt
  └─ stop() ─► unload_all ─► shutdown() ─► {"cmd":"stop"} ─► exit 0 (sinon kill)
```

* **Pas de `prctl(PR_SET_PDEATHSIG)`** : l'hôte lit son `stdin` ; quand `holafd`
  meurt — y compris par `SIGKILL` — le noyau ferme le tube, l'hôte lit EOF et
  sort.  C'est plus robuste qu'un signal (et un `PDEATHSIG` est lié au *thread*
  qui a forké, donc fragile dans un parent multi-thread).  Testé :
  `fx_host_dies_with_its_daemon_on_sigkill`.
* Les **noms de nœuds dérivent de l'uid** : après un respawn le graphe retrouve
  les mêmes noms, donc les liens peuvent être refaits sans rien reconfigurer.
* Un hôte « qui ne répond pas » (10 s) est une erreur, pas un blocage : le
  worker de `holafd` ne se fige jamais sur `read()`.

## 5. Priorité temps réel — ce qui est possible

L'hôte est un **client PipeWire ordinaire** : il appelle
`pw_context_new(mainloop, NULL)`, donc il charge la configuration client par
défaut — celle que charge `pw-cli`.  La promotion des threads *data-loop* est
faite par `libpipewire-module-rt` **si et seulement si** la machine la demande
(rtkit disponible, ou `PIPEWIRE_CONFIG_NAME=client-rt.conf`).  Rien n'est fait
en plus, et c'est volontaire : le comportement RT de l'hôte est celui de
`pw-cli`, par construction.

Ce qui est possible si besoin, sans changer ce défaut :

* `PIPEWIRE_CONFIG_NAME=client-rt.conf` dans l'environnement de l'app (hérité
  par l'hôte, qui est un enfant) : force `module-rt` avec `rt.prio` /
  `rt.time.soft` ;
* `HOLAFD_BIN` : chemin explicite du binaire utilisé comme hôte (utile en
  packaging).

Note : le processus enfant hérite de `RUST_LOG`, donc ses logs arrivent dans le
même flux que ceux de `holafd`.

## 6. Sélection et repli

| moyen | valeur |
|---|---|
| CLI `holafd` | `--fx-host-mode external` (défaut) / `inprocess` |
| env | `HOLAF_FX_HOST=external|inprocess` |
| `status` | `status.fx_host = {mode, pid, respawns, chains}` |

Le repli `inprocess` est bruyant (`log::error!`) : dégrader silencieusement vers
l'architecture qui hache rendrait le prochain rapport de bug indiagnosticable.

## 7. Banc de mesure (`tools/audio_bench.py`)

```bash
eval "$(./scripts/start-pipewire-headless.sh --export)"
python3 tools/audio_bench.py matrix \
    --scenarios ref,pwcli,holafd,fxhost --quantums 128,64 --repeats 3 --seconds 8
```

* `ref` : pas de chaîne (contrôle) · `pwcli` : **cas A**, chaîne dans un
  `pw-cli` dédié (moteur Python) · `holafd` : **cas B**, chaîne in-process
  (`--fx-host-mode inprocess`) · `fxhost` : **cas C**, hôte dédié (défaut).
* métriques par échantillon : `holes` (|x| lissé < 2 % de la médiane pendant
  ≥ 0,25 ms — un trou de service), `steps` (saut > 1,5 × la pente maximale du
  1 kHz — un clic), `THD+N`, `ERR` (compteur `pw-top` de la dernière trame),
  et `host%` / `daemon%` (CPU du processus hôte / du démon).
* `--poll N` : ajoute un plan de contrôle `holafctl status` à N Hz (ce que fait
  l'UI) — c'est la différence structurelle entre B et C ;
* `--load N` : N boucles `yes`.  **Attention** : saturer tous les cœurs casse
  la mesure elle-même (voir §8).
* `--device-channels N` : la chaîne alimente un device null-sink à **N canaux**
  (`4` = une interface USB 4 sorties, voir §8 bis) au lieu du bus stéréo du
  strip ; la capture se fait sur le monitor de ce device.
* Le nom du bus du strip de banc est **demandé au démon**
  (`holafctl status` → `strips[].node_name`), donc le banc suit la convention
  de nommage (`Holaf_Bus_Bench`) sans la coder en dur.

## 8. Ce que le banc de ce conteneur peut — et ne peut pas — prouver

Mesuré ici (Debian 12, PipeWire 0.3.65 headless, 12 cœurs partagés) :

| quantum | cas | trous / 8 s (min – max sur 3 essais) |
|---|---|---|
| 128 | `ref` (aucune chaîne) | 1 – 9 (méd. 2) |
| 128 | `pwcli` (cas A) | 4 – 14 (méd. 9) |
| 128 | `holafd` (cas B, in-process) | 4 – 11 (méd. 6) |
| 128 | `fxhost` (cas C, défaut) | 5 – 13 (méd. 8) |
| 64 | `ref` | 13 – 28 (méd. 16) |
| 64 | `pwcli` | 26 – 83 (méd. 27) |
| 64 | `holafd` | 9 – 29 (méd. 29) |
| 64 | `fxhost` | 22 – 51 (méd. 27) |
| 32 | `ref` | 26 – 38 |
| 32 | `pwcli` | 18 – 66 |
| 32 | `fxhost` | 30 – 54 |

Toutes les plages se recouvrent, `ref` compris : à quantum ≤ 64 le plancher du
conteneur (~30 trous / 8 s à quantum 32) dépasse toute différence entre les cas.

**Conclusion honnête : ce conteneur ne peut pas départager A, B et C.**  La
raison est mesurable :

* aucun client n'y est temps réel (`/proc/self/limits` → `Max realtime priority
  0`, pas de `rtkit-daemon`) : tout tourne en `SCHED_OTHER` ;
* l'hôte est partagé (load average ~7/12) ;
* surtout, le **graphe sans aucune chaîne** (`ref`) perd autant de trames que
  les cas avec chaîne : le plancher de bruit du conteneur domine, et `pwcli` —
  l'architecture qui ne hache **pas** sur la machine de l'utilisateur — n'y fait
  pas mieux que l'in-process.

Ce que le banc prouve en revanche, et qui est indépendant de l'ordonnanceur :

* `host%` / `daemon%` : le CPU du démon passe de **6,3 % à 0,9 %** (§2.2) —
  c'est bien le thread qui sert la chaîne qui est libéré ;
* `THD+N` et le niveau sont identiques dans les quatre cas (le DSP est le
  même) ;
* `degraded=false`, `pending=[]`, `ERR=0` dans tous les cas : le graphe et le
  service sont sains.

## 8 bis. Piste écartée par la mesure : device 4 canaux vs strips stéréo

Hypothèse à tester : `alsa_output.usb-PreSonus_Studio_26c_…-00.pro-output-0` est
un nœud **4 canaux** (`s32le 4ch 48000Hz`) alors que tous nos strips et chaînes
sont stéréo (`float32le 2ch`) ; la négociation 2 → 4 canaux pourrait provoquer un
remix, voire un hachage (le casque hache, les HP C-Media 2 canaux non).

### Ce que font les deux moteurs (identique)

* Ils ciblent les **deux premiers ports du device** : `playback_FL` et
  `playback_FR`.  Côté Rust, `link_nodes` prend les ports du nœud par
  `audio.channel` (`FL`/`FR`, repli sur l'index 0/1) ; côté Python,
  `_auto_link_ports` matche `FL`/`left` en priorité puis `:output_0`.  Sur un
  device `FL FR RL RR`, `RL`/`RR` restent **non liés** dans les deux cas.
* Aucun remix : la chaîne déclare `audio.channels = 2`,
  `audio.position = [ FL, FR ]` et `stream.dont-remix = true` (config SPA-JSON
  identique au caractère près, cf. `spa_config::build_filter_chain_config`).
* Le volume : Rust écrit `channelVolumes`/`monitorVolumes` avec **2 valeurs**,
  Python utilise `pactl set-sink-volume <nœud> N%`.  Mesuré sur un device 4
  canaux : PipeWire (adapter audioconvert) étend le tableau — on relit
  `channelVolumes = 0.512 ×4` pour un volume de 80 %, exactement ce que laisse
  `pactl … 80%`.  Pas d'écart.

### Mesure en headless (2 canaux vs 4 canaux)

Même graphe dans les deux cas (bus d'entrée → chaîne EQ (hôte `--fx-host`) →
device), le seul paramètre qui change est le nombre de canaux du device
(`audio.channels=4`, `audio.position=[ FL FR RL RR ]`), tonalité 1 kHz, capture
du monitor du device :

| device | ports liés | trous | steps | THD+N | rms |
|---|---|---|---|---|---|
| 2 canaux (défaut) | `FL`, `FR` | 0 | 0 | −81,7 dB | 0,250 |
| 4 canaux | `FL`, `FR` (+ `RL`/`RR` libres) | 0 | 0 | −81,7 dB | 0,250 |

Identique au chiffre près.  À quantum forcé à 64, les deux se dégradent **de la
même façon** (conteneur sans temps réel, cf. §8) : 4 canaux n'est pas pire.

**Conclusion : la largeur du device n'est pas la cause du hachage.**  Elle ne
change ni le câblage, ni le format négocié, ni le DSP ; `pw-top` ne montre aucun
`ERR` et le monitor du device 4 canaux restitue la tonalité sans trou.

### Reproduire / mesurer sur la machine de l'utilisateur

```bash
# 1) quels ports du casque sont réellement liés (attendu : FL et FR seulement)
pw-link -l | grep -A1 'pro-output-0' 

# 2) le format négocié de chaque côté de la chaîne (attendu : f32 2ch partout
#    côté chaîne, le device garde son 4ch)
pw-top -b -n 1 2>/dev/null | grep -E 'Holaf|pro-output'

# 3) compter les xruns par nœud, EQ actif puis inactif (colonne ERR)
pw-top            # laisser tourner 10 s dans chaque configuration

# 4) banc A/B avec un device 4 canaux (le seul paramètre qui change)
eval "$(./scripts/start-pipewire-headless.sh --export)"   # ou session réelle
python3 tools/audio_bench.py run --scenario fxhost --device-channels 4 --seconds 8
python3 tools/audio_bench.py run --scenario fxhost --device-channels 2 --seconds 8

# 5) la même chose sans chaîne du tout (plancher de bruit local)
python3 tools/audio_bench.py run --scenario ref --seconds 8
```

Si `ERR` reste à 0 sur le nœud `pro-output-0` et que le monitor 4 canaux ne
contient pas de trous alors que l'oreille entend le hachage, la cause est en
aval du graphe (périodes ALSA/USB, `api.alsa.period-size`, ou l'interface
elle-même) — et non dans la négociation stéréo → 4 canaux.

## 9. À vérifier sur la machine de l'utilisateur (la seule qui reproduit)

1. Lancer l'app normalement, puis :

   ```bash
   ./holafctl --socket /tmp/holaf-$USER.sock status | grep -A5 fx_host
   ps -o pid,ppid,args -p "$(pgrep -f -- '--fx-host' | head -1)"
   ```

   Attendu : `"mode": "external"`, un `pid` **différent** de celui de `holafd`,
   et un processus enfant `holafd --fx-host`.

2. Écouter le casque PreSonus avec l'EQ actif au lancement : plus de hachage.
3. Comparer en une commande, sans toucher au code :

   ```bash
   HOLAF_FX_HOST=inprocess python main.py     # ancien comportement (hache ?)
   HOLAF_FX_HOST=external  python main.py     # nouveau comportement
   ```

4. Vérifier la propreté : après fermeture de l'app,
   `pgrep -af -- '--fx-host'` ne doit rien renvoyer, et
   `pw-cli ls Node | grep Holaf_` non plus.
5. En cas de doute sur le CPU, `top -p $(pgrep -d, -f holafd)`.
