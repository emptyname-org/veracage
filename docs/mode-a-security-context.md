# Mode A: `wp_security_context_v1` (deferred implementation spec)

**Status: not implemented, intentionally.** Veracage runs its own compositor
(`veracage-compositor`) as a separate instance, which already delivers the
clipboard isolation Mode A would not. This document preserves the full, verified
implementation plan in case the trade-off ever changes. The rationale for
deferring it is in the next section.

## Why deferred (the security finding)

`wp_security_context_v1` lets a compositor **identify** sandbox connections
and **deny them privileged globals** (`wlr-data-control`, screencopy,
virtual-keyboard/pointer, layer-shell, ext-session-lock, ...). It does **NOT**,
by protocol, partition the `wl_data_device` selection, so it does **not**
make the sandbox and host clipboards independent. Veracage's core clipboard
goal (a copy in the sandbox must not reach the host clipboard) is delivered
by the **separate compositor instance**, not by
Mode A. Adopting Mode A as default would *weaken* clipboard isolation.

Also: **GNOME/Mutter does not implement the protocol** (the implementation MR
was closed unmerged). KWin >= 6 and wlroots >= 0.17 (sway >= 1.9),
Weston >= 13, Hyprland/COSMIC/niri/river/labwc do.

## If revisited: the implementation (verified against protocol XML + KWin/wlroots source)

### Lifetime (the key fact)

The creating process and its Wayland connection do **not** need to stay
alive. After `commit` you may destroy the objects and disconnect. The
compositor keeps accepting sandbox connections. Lifetime is governed solely
by the **`close_fd`**: the compositor watches it for hang-up only. Hold the
**write end** of a pipe open == context alive. Close it == teardown. The
listening socket inode must stay on disk for clients to `connect()`.

### Message sequence (both interfaces are v1)

```
wl_registry.bind("wp_security_context_manager_v1", version=1)      -> MGR
MGR.create_listener(new_id, listen_fd, close_fd)                   -> CTX
CTX.set_sandbox_engine("org.veracage")   # reverse-DNS, NON-empty (req by KWin)
CTX.set_app_id("org.kde.kate")           # NON-empty (req by KWin)
CTX.set_instance_id(<uuid>)              # optional
CTX.commit()
CTX.destroy(); MGR.destroy()             # legal post-commit; does NOT stop listening
roundtrip()                              # FLUSH so requests + fds reach the compositor
```

- `listen_fd`: the client does `socket(AF_UNIX, SOCK_STREAM)` + `bind(path)` +
  `listen()` **before** `create_listener` (wlroots checks `SO_ACCEPTCONN`).
- `close_fd`: pass the **read** end of `pipe2(O_CLOEXEC)`. Keep the **write**
  end open for the session.
- Engine string is free-form reverse-DNS. Do **not** use `org.flatpak` (it
  implies a `$XDG_RUNTIME_DIR/.flatpak/$instance_id/info` contract) or
  `bubblewrap` (no such registered engine).
- After `commit`, only `destroy` is valid (else `already_used`).

### Recommended architecture

A one-shot Rust binary `veracage-secctx` that receives a pre-created listen
fd + close-pipe read fd (via `pass_fds`), does the handshake, and exits. The
persistent host process (session leader / agent) creates the socket + pipe,
holds the close-pipe **write** fd as the lifetime token, bind-mounts the
socket into bwrap at `/run/user/$UID/wayland-0`, and closes the write fd on
teardown.

Capability detection: bind `wp_security_context_manager_v1` v1. `NotPresent`
=> fall back to Mode B.

### Rust crate

```toml
wayland-client    = "0.31"
wayland-protocols = { version = "0.32", features = ["staging", "client"] }
```
`create_listener(listen_fd: BorrowedFd, close_fd: BorrowedFd, qh, udata) -> WpSecurityContextV1`,
module `wayland_protocols::wp::security_context::v1::client`.

### If adopted, it would be **opt-in** (config `wayland_mode`), never the
default, and documented as: deny-privileged-globals + window-integration UX,
**shared clipboard**. Verify the specific compositor's selection behaviour
before relying on any clipboard separation.
