# Veracage — Architecture

A thin wrapper that mounts a VeraCrypt volume into a kernel-isolated namespace and launches applications inside a bubblewrap sandbox with Wayland clipboard isolation.

---

## Goals

- Decrypted volume contents invisible to host OS processes (indexers, backup daemons, etc.)
- Applications opening files from the volume run in an isolated sandbox
- Clipboard not shared with host unless explicitly requested by the user
- VeraCrypt handles all crypto — no fork, no custom crypto code

---

## Components

```
┌─────────────────────────────────────────────────────┐
│                   veracage (Python)              │
│                                                     │
│  1. mount        veracrypt --text --mount <vol>     │
│                  → /tmp/vs-XXXX (host mountpoint)   │
│                                                     │
│  2. isolate      unshare --mount                    │
│                  bind-mount into private namespace  │
│                  unmount host-visible mountpoint    │
│                                                     │
│  3. sandbox      bwrap                              │
│                  --ro-bind /usr /usr                │
│                  --bind /run/vs-XXXX /vault         │
│                  --wayland-socket $WAYLAND_DISPLAY  │
│                  -- <app>                           │
│                                                     │
│  4. cleanup      on exit: veracrypt --dismount      │
│                            namespace destroyed      │
└─────────────────────────────────────────────────────┘
```

---

## Mount Isolation

- `veracrypt --text --mount` creates a dm-crypt device and mounts it at a temp path
- `unshare --mount` forks into a new mount namespace
- The decrypted mount is bind-mounted inside the namespace; the host-visible mountpoint is unmounted
- Result: the decrypted filesystem does **not** appear in `/proc/mounts` for any process outside the namespace
- Caveat: the `/dev/mapper/veracryptN` device mapper entry remains visible to root — the kernel does not namespace dm-crypt devices

---

## Sandbox (bubblewrap)

Each launched application gets a bwrap sandbox with:

| Resource | Policy |
|---|---|
| Filesystem | Read-only system paths + `/vault` (the decrypted volume) only |
| Home directory | Not mounted |
| Network | Blocked by default; opt-in per invocation |
| Wayland socket | Forwarded (app can render windows) |
| Clipboard | Isolated — see below |

---

## Clipboard Isolation (Wayland)

On Wayland, clipboard access requires the compositor to mediate transfers. The sandbox uses this:

- The sandboxed app's Wayland socket is forwarded, so it can display windows normally
- The compositor enforces that clipboard reads only succeed when the requesting app has focus — no background snooping
- For explicit host ↔ sandbox clipboard sharing, the user triggers a one-shot transfer via a hotkey that invokes `wl-paste | wl-copy` across the boundary

No nested compositor or Xephyr needed — this is a native Wayland property.

---

## Application Launcher

The user configures a list of allowed apps (e.g. `mousepad`, `okular`, `thunar`). The launcher:

1. Presents a file picker scoped to `/vault`
2. Detects MIME type
3. Launches the appropriate app inside bwrap with the file as argument

Multiple apps can run simultaneously inside the same namespace/sandbox session.

---

## Lifecycle

```
veracage open <volume.vc>
    │
    ├─ prompt password
    ├─ veracrypt mount → /tmp/vs-PID
    ├─ unshare: new mount namespace
    ├─ bind-mount /tmp/vs-PID → namespace:/vault
    ├─ unmount /tmp/vs-PID from host
    ├─ launch shell or app picker inside bwrap
    │
    └─ on exit (all sandboxed processes done):
           veracrypt --dismount
           namespace torn down automatically
```

---

## Non-Goals

- Cross-platform support (Linux/Wayland only)
- Custom encryption (VeraCrypt handles this entirely)
- Persistent sandboxed desktop environment
