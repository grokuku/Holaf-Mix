//! Minimal FFI shim ("holaf-sys") over the parts of the C PipeWire API that
//! `pipewire-rs` does **not** expose.
//!
//! pipewire-rs 0.8 exposes no client-side module loader: `pw_core_methods`
//! only has `create_object`/`destroy`, there is no `load_module` at the wire
//! protocol, and `pipewire::module::Module` is a read-only proxy over remote
//! `Module` globals.
//!
//! The C function that loads a module **into a local context** is:
//!
//! ```c
//! struct pw_impl_module *
//! pw_context_load_module(struct pw_context *context,
//!                        const char *name, const char *args,
//!                        struct pw_properties *properties);
//! ```
//!
//! (declared in `impl-module.h`, bound by `pipewire-sys`).
//!
//! J1 phase-1 recon established *why* this is the right call for Holaf-Mix:
//!
//! - the LADSPA filter-chain module (`libpipewire-module-filter-chain`) must
//!   live **in the same process** as the node that drives it.  `pw-cli`
//!   achieves this by loading the module into its *own* local context; only
//!   the module's two streams (`input.*` / `output.*`) are exported to the
//!   remote daemon;
//! - holafd must therefore host a local `pw_context` (which `pipewire-rs`'s
//!   [`pipewire::context::Context`] already is — it wraps a real
//!   `pw_context_new` and exposes `as_raw_ptr()`) and load the module there,
//!   exactly like `pw-cli`.
//!
//! This module provides:
//!
//! - [`load_module`] — load a module into the caller's context
//!   (returns a [`LocalModule`] handle).
//! - [`set_context_core`] / [`get_context_core`] — optional wiring so a
//!   module that looks up the context default core (filter-chain does) can
//!   reuse **our** connection instead of silently opening a second one.
//! - [`LocalModule::destroy`] / [`LocalModule::schedule_destroy`] — explicit
//!   unload during the same run (cleanup), before the context goes away.
//!
//! # Safety model
//!
//! All functions here are `unsafe`: they take raw pointers across the FFI
//! boundary and touch library-owned objects that Rust cannot track.  Callers
//! must honour the following contract:
//!
//! - the `pipewire::context::Context` (and its connected `Core`, if set via
//!   [`set_context_core`]) must stay alive for the whole lifetime of the
//!   returned [`LocalModule`];
//! - the module pointer must only be used through the returned handle (no
//!   copies, no aliasing);
//! - the main loop that owns the context must be iterated while module
//!   events (streams/ports appearing, destroy callbacks) are expected;
//! - [`LocalModule::destroy`] must not be called from inside a PipeWire
//!   callback of the same module (use [`LocalModule::schedule_destroy`]).

use std::ffi::{CStr, CString};
use std::ptr;

use pipewire as pw;
use pw::sys as pw_sys;
use thiserror::Error;

/// Interface type string of the context default core, as expected by
/// `pw_context_set_object(context, PW_TYPE_INTERFACE_Core, core)`.
const PW_TYPE_INTERFACE_CORE: &[u8] = b"PipeWire:Interface:Core\0";

/// Errors raised by the small FFI surface.
#[derive(Debug, Error)]
pub enum SysError {
    /// `pw_context_load_module` returned `NULL` (load failed).  The errno
    /// captured right after the call is carried for diagnostics.
    #[error("pw_context_load_module('{name}') failed (errno={errno}): {message}")]
    LoadModuleFailed {
        name: String,
        errno: i32,
        message: String,
    },
}

/// A handle to a module loaded into our local `pw_context`.
///
/// Wraps the `*mut pw_impl_module` returned by `pw_context_load_module`.
/// **Not** `Clone`/`Copy` on purpose: the pointer must be owned exactly once
/// and destroyed at most once.
pub struct LocalModule {
    ptr: *mut pw_sys::pw_impl_module,
}

impl LocalModule {
    /// Pointer to the underlying `pw_impl_module` (for listeners / FFI).
    ///
    /// # Safety
    ///
    /// The caller must not outlive the returned reference with the module.
    pub fn as_raw_ptr(&self) -> *mut pw_sys::pw_impl_module {
        self.ptr
    }

    /// Register the module in the local context registry (id used for
    /// `pw-cli ls`-style local lookups).  PipeWire does this automatically
    /// when the module registers its global; kept for completeness.
    ///
    /// # Safety
    ///
    /// `module` must be a live handle from [`load_module`].
    pub unsafe fn global(&self) -> *mut pw_sys::pw_global {
        pw_sys::pw_impl_module_get_global(self.ptr)
    }

    /// Immediately destroy the module and all its objects (streams, graph).
    ///
    /// # Safety
    ///
    /// Must not be called from within a callback belonging to this module
    /// (or its streams) while the context main loop is iterating on the same
    /// thread — use [`LocalModule::schedule_destroy`] instead.  The handle
    /// must not be used afterwards.
    pub unsafe fn destroy(self) {
        // Safety: `self.ptr` is a valid module pointer and `self` consumes it
        // exactly once.
        pw_sys::pw_impl_module_destroy(self.ptr);
    }

    /// Ask the context to destroy the module on its next iteration
    /// (deferred; safe from callbacks).  Callers must then iterate the main
    /// loop for the destroy to take effect.
    ///
    /// # Safety
    ///
    /// Same lifetime rules as [`LocalModule::destroy`]; after this call the
    /// handle must not be used and its `pw_impl_module` will be freed by the
    /// context main loop.
    pub unsafe fn schedule_destroy(&self) {
        // Safety: `self.ptr` is a valid module pointer.
        pw_sys::pw_impl_module_schedule_destroy(self.ptr);
    }
}

/// Load a module into `context`'s local context (the in-process, pw-cli-style
/// hosting path).  See the module docs for *why* filter-chain needs this.
///
/// # Parameters
///
/// - `context`: any live `pipewire::context::Context` (its underlying
///   `pw_context` is passed straight through).  It must stay alive while the
///   returned module lives.
/// - `name`: module file name, e.g. `"libpipewire-module-filter-chain"`.
/// - `args`: SPA-JSON module argument string, e.g. the `filter.graph` /
///   `capture.props` / `playback.props` config built by
///   [`crate::spa_config::build_filter_chain_config`].  `None` is allowed
///   when the module needs no arguments.
/// - `properties`: optional extra `pw_properties` (rarely needed; modules
///   take their config through `args`).  Pass `None`.
///
/// # Returns
///
/// A [`LocalModule`] handle on success.
///
/// # Safety
///
/// - `context` must be a *live, un-dropped* [`pipewire::context::Context`];
///   the returned module inherits its main loop and support objects.
/// - The caller must guarantee the returned module outlives nothing that
///   would drop `context` (or its connected core) first.
pub unsafe fn load_module(
    context: &pw::context::Context,
    name: &str,
    args: Option<&str>,
    properties: Option<&pw::properties::Properties>,
) -> Result<LocalModule, SysError> {
    let name_c = CString::new(name).expect("NUL byte in module name");
    let args_c = args.map(|a| CString::new(a).expect("NUL byte in module args"));
    let props_ptr = match properties {
        Some(p) => p.as_raw_ptr(),
        None => ptr::null_mut(),
    };

    // SAFETY:
    // - `context.as_raw_ptr()` is a valid `pw_context` for the lifetime of
    //   `context` (kept alive by the caller).
    // - `name_c`/`args_c` are NUL-terminated C strings valid for the call.
    // - `pw_context_load_module` either returns a module owning copies of the
    //   args/properties, or NULL.
    let module = pw_sys::pw_context_load_module(
        context.as_raw_ptr(),
        name_c.as_ptr(),
        args_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
        props_ptr,
    );

    if module.is_null() {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(SysError::LoadModuleFailed {
            name: name.to_string(),
            errno,
            message: std::io::Error::last_os_error().to_string(),
        });
    }

    Ok(LocalModule { ptr: module })
}

/// Store `core` as the context's default core object
/// (`pw_context_set_object(PW_TYPE_INTERFACE_Core)`).
///
/// Modules that look up the default core (the filter-chain does, see
/// `module-filter-chain.c`: `impl->core = pw_context_get_object(context,
/// PW_TYPE_INTERFACE_Core)`) will then **reuse our connection** instead of
/// silently opening a second one to the daemon.  This is what holafd wants:
/// one client, owned and shut down by us.
///
/// # Safety
///
/// - `context` and `core` must be alive; `core` must be a `pw_core` proxy
///   returned by `context.connect(...)` (i.e. of the same context).
/// - The core must outlive any module loaded afterwards that reuses it.
pub unsafe fn set_context_core(context: &pw::context::Context, core: &pw::core::Core) {
    let type_c = CStr::from_bytes_with_nul_unchecked(PW_TYPE_INTERFACE_CORE);
    // SAFETY: type is a static NUL-terminated string; `core.as_raw_ptr()`
    // points to the live core; `context` stores a raw pointer only.
    pw_sys::pw_context_set_object(
        context.as_raw_ptr(),
        type_c.as_ptr(),
        core.as_raw_ptr().cast(),
    );
}

/// Fetch the stored default core object, if any (see [`set_context_core`]).
///
/// # Safety
///
/// `context` must be alive.  The returned pointer (when non-null) belongs to
/// the caller's core and must not be dereferenced past the core's lifetime.
pub unsafe fn get_context_core(context: &pw::context::Context) -> *mut pw_sys::pw_core {
    let type_c = CStr::from_bytes_with_nul_unchecked(PW_TYPE_INTERFACE_CORE);
    // SAFETY: type is a static NUL-terminated string.
    pw_sys::pw_context_get_object(context.as_raw_ptr(), type_c.as_ptr()).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_core_string_is_nul_terminated() {
        // SAFETY: PW_TYPE_INTERFACE_CORE is a static NUL-terminated literal.
        let c = unsafe { CStr::from_bytes_with_nul_unchecked(PW_TYPE_INTERFACE_CORE) };
        assert_eq!(c.to_str().unwrap(), "PipeWire:Interface:Core");
    }

    #[test]
    fn error_type_is_debuggable() {
        let e = SysError::LoadModuleFailed {
            name: "x".into(),
            errno: 0,
            message: "y".into(),
        };
        assert!(format!("{e:?}").contains("LoadModuleFailed"));
    }
}
