# ADR 0010: Trusted identity for the shell, lock and portal backends under one UID

- Status: Proposed (#1162); owner decision pending
- Date: 2026-11-16
- Owner: project (decision pending)
- Dependent speks: 004 (secure session), 005 (extensions)
- Related: [ADR 0005](0005-control-socket-peer-authentication.md) (control socket),
  [ADR 0011](0011-lock-auth-and-capture-grant-lifecycle.md) (lifecycles),
  [security model](../security-model.md), issue #1162

## Context

Every process in a Tuna Desktop session runs as the session user. The
compositor, the shell, the portal backend, gnome-remote-desktop, the PAM
helper and every unsandboxed application share one UID. The kernel's
discretionary access control does not separate them. A same-UID process
can read `/proc/<pid>/environ`, connect to every session socket, own any
unowned well-known D-Bus name and, if Yama allows it, `ptrace` its peers.

The compositor holds the session's authority: focus, stacking, input
routing, the lock flag, capture authorization and privileged Wayland
globals. It must give parts of that authority to other processes. To do
that safely it must know which process is which. The facts it can get are:

- the kernel's peer credentials of a socket (`SO_PEERCRED` UID and PID at
  connect time, and `SO_PEERPIDFD`, a descriptor pinned to that process);
- the `Child` handles and pidfds of processes it started itself;
- the credentials the bus daemon reports for a D-Bus sender;
- the executable that `/proc/<pid>/exe` names, and that file's owner and mode.

It cannot tell whether a trusted process was started with a hostile
environment, has a debugger attached or has loaded injected code.

[ADR 0005](0005-control-socket-peer-authentication.md) decided the control
socket only. The [ADR register](README.md#decision-register) kept the rest of
the same-UID threat model, and the identity of the lock and portal clients,
open as a 1.0 release blocker. This ADR records what the code on `main`
does for every trusted identity, and proposes it as the 1.0 boundary.

## Decision (proposed)

**Boundary.** Native same-UID processes are out of scope, as in GNOME 51.
A native process running as the session user can start the genuine,
root-owned portal or grd binary with `LD_PRELOAD` and act as it; it needs
no `ptrace` and Yama does not stop it. No check of process attributes
(PID, pidfd, executable, bus name) can tell that process from the real
one, so this ADR claims no defence against it (review finding F1).

In scope are callers that are not native same-UID processes: sandboxed
applications (Wayland clients in a `wp_security_context_v1`, and Flatpak
apps whose session-bus access is filtered), processes of other users, and
a person at the locked screen. Against them the identity checks
below are the boundary. Against native same-UID processes they are defence
in depth: they refuse a process that only *claims* an identity (a bus name,
an app ID, a socket connection, a copied binary), and so raise the cost of
an attack. GNOME 51 makes the same out-of-scope choice with weaker checks
(see [Comparison with GNOME 51](#comparison-with-gnome-51)).

Mitigations being built for the native same-UID case (not on `main`):

- a GNOME-style screen-sharing and remote-access indicator with a Stop
  control, so that no capture, remote input or input capture is silent (F2);
- gnome-remote-desktop sessions admitted only while Remote Desktop is
  enabled in gsettings, since that path has no portal dialog (F1, item 3);
- pidfd-pinned identity for the shell's session-lock request and D-Bus
  calls (F4).

The compositor recognises the identities below. Any other client is an
ordinary client with authority only over its own surfaces. Paths are
relative to the repository root.

### Supervised shell

- **Recognised by parentage.** The compositor spawns the shell as its own
  child and holds the `Child` unreaped, so the shell's PID cannot be
  recycled while it is trusted (`crates/compositor/src/supervise.rs`,
  `Supervisor::child_pid`, `ShellDriver::child_pid`). The binary comes from
  `--shell-bin`, which `tuna-session` pins to `resolve_shell_bin`:
  `TUNA_SHELL_BIN`, else a `tuna-shell-gtk`/`tuna-shell-host` sibling of the
  compositor, else `PATH` (`crates/compositor/src/runtime.rs`,
  `resolve_shell_bin_in`; `crates/compositor/src/bin/tuna-session.rs`,
  `session_argv`). The binary's owner, mode and path are not checked.
- **Wayland connection.** On accept, `ClientState::native_connection` reads
  `SO_PEERCRED` and `SO_PEERPIDFD` (`crates/compositor/src/capture_security.rs`,
  `socket_credentials`). `ClientState::admit_supervised_shell`
  (`crates/compositor/src/lib.rs`) marks the connection as the shell's only
  when the pidfd-pinned PID (`pinned_process_id`) equals the held child's
  PID (`crates/compositor/src/layer.rs`, `is_shell_peer`). Without a pidfd it
  falls back to the numeric `SO_PEERCRED` PID. The flag is set once, at
  accept.
  - May bind `zwlr_layer_shell_v1` and `ext_foreign_toplevel_list_v1`. Both
    globals are filtered by `ClientState::privileged(.., supervised_shell)`
    (`lib.rs` layer-shell state; `crates/compositor/src/protocols.rs`
    `foreign_toplevels`).
  - Evidence: `crates/compositor/tests/protocols.rs`
    (`layer_shell_is_visible_and_bindable_only_by_the_supervised_shell`,
    `the_shell_lists_toplevels_until_they_close_and_apps_cannot_bind_the_list`);
    `layer.rs` (`only_the_held_shell_child_is_a_shell_peer`);
    `capture_security.rs`
    (`native_socket_identity_pins_the_connecting_process_and_expires_on_exit`,
    `process_identity_requires_a_pin_and_does_not_survive_peer_exit`). CI:
    `check` job.
- **Control socket.** As ADR 0005. Each tick the runtime sets
  `PeerGate::Pid(child)` or `PeerGate::Closed` and binds the auth owner to
  the child's PID and supervisor generation (`runtime.rs`, `Runtime::tick`;
  `crates/compositor/src/control.rs`, `PeerGate::admits`,
  `ControlHub::set_auth_owner`). A narrowed gate evicts live sessions and
  rechecks pending handshakes (`ControlHub::poll`, `advance_pending`).
  Evidence: `crates/compositor/tests/peer_auth.rs`.
- **Session bus.** `Authority::authenticate_sender` treats a sender as the
  shell when the bus's `GetConnectionUnixProcessID` equals the published
  child PID (`capture_security.rs`; published by `Authority::publish` from
  `Runtime::tick` and `Runtime::engage_lock`). This admits ScreenCast,
  Screenshot, RemoteDesktop and Introspect calls (`Authority::admit`,
  `admit_remote`), and relaying `org.gnome.Shell.Introspect` for another
  caller (`admit_forwarded`). It does not admit InputCapture
  (`admit_portal`).

### Lock client

The lock client is the supervised shell. `ext_session_lock_manager_v1` is
visible to every client outside a security context
(`crates/compositor/src/session_lock.rs`, `LockProtocol::new`).

- A lock is granted only when `lock_request_verdict` matches the
  requester's `SO_PEERCRED` PID, read through
  `Client::get_credentials` when the lock is requested, with the held shell
  PID, and the disable-lock-screen lockdown allows it (`session_lock.rs`,
  `SessionLockHandler::lock`, `lock_request_verdict`; `runtime.rs`, the
  `take_lock_request` branch of `Runtime::tick`). This check uses the
  numeric PID, not the pidfd-pinned flag; see risk R2.
- Only the approved client's surfaces are drawn or take input
  (`State::approved_lock_surface`, `lock_surface_for`). A release from an
  unapproved lock object is a protocol error (`Dispatch<ExtSessionLockV1>`).
  A client's `unlock_and_destroy` never unlocks; see ADR 0011.
- Evidence: `session_lock.rs` unit tests (`only_the_supervised_shell_pid_may_lock`,
  `an_unknown_pid_is_never_the_shell`,
  `lockdown_policy_refuses_even_the_shell_and_reports_why`);
  `crates/compositor/tests/session_lock_protocol.rs`;
  `crates/compositor/tests/session_lock_runtime.rs`
  (`actual_runtime_grants_only_the_shell_lock_and_input_reaches_only_its_curtain`,
  CI `check` step "Session lock runtime proof").

### Portal backend (`xdg-desktop-portal-gnome`)

Two separate checks:

- **Display bootstrap.** `org.gnome.Mutter.ServiceChannel` opens a Wayland
  connection of type Portal only for a same-UID caller whose
  `/proc/<pid>/exe` is the canonical `/usr/libexec/xdg-desktop-portal-gnome`
  or `/usr/lib/xdg-desktop-portal-gnome`, owned by root and not group- or
  world-writable (`capture_security.rs`, `Authority::admit_service_connection`,
  `installed_executable`). The connection gets `mutter_x11_interop` and no
  capture authority (`crates/compositor/src/mutter.rs`,
  `open_wayland_service_connection`; `crates/compositor/src/x11_interop.rs`,
  `authorized`). Nautilus (file chooser) and the global-shortcuts provider
  pass the same check for their own type only (`ServiceClient::executables`).
- **Capture authority.** The sender must currently own
  `org.freedesktop.impl.portal.desktop.gnome` *and* pass the executable check
  (`Authority::authenticate_sender`). Owning the name alone is not enough,
  and a copied binary fails the root-owner check.
- Evidence: `capture_security.rs` (`arbitrary_process_cannot_be_the_installed_portal`,
  `typed_service_roles_do_not_accept_an_arbitrary_process`);
  `crates/compositor/src/mutter/service_channel_tests.rs`
  (`x11_interop_registry_requires_typed_admission_not_service_liveness`);
  CI `portal-security` (`scripts/tuna-portal-security`,
  `tests/portal-reference/run.sh`), which drives the installed GNOME portal
  stack and checks untrusted and spoofed callers are denied.

### Remote-desktop daemon (gnome-remote-desktop)

Only `RemoteDesktop.CreateSession` and the ScreenCast session linked to it
use `Authority::admit_remote`. It admits the shell or the portal, or else a
same-UID caller whose executable is the root-owned
`/usr/libexec/gnome-remote-desktop-daemon` or
`/usr/lib/gnome-remote-desktop-daemon` and which currently owns
`org.gnome.RemoteDesktop.User`, `.Handover` or `.Headless`
(`capture_security.rs`, `remote_desktop_daemon`). The daemon's configured
credentials are the consent, as in GNOME. Evidence: `capture_security.rs`
(`remote_desktop_daemon_identity_is_the_root_owned_installed_binary`);
`crates/compositor/src/mutter/remote_bus_tests.rs`
(`only_the_installed_remote_desktop_daemon_may_create_sessions`); CI
`remote-security` (portal path and untrusted denial). The positive grd path
has no CI run; its evidence is the manual Marlin receipt in
`docs/marlin/2026-10-30-rdp/`.

### PAM helper

The compositor does not accept the helper as a peer; it creates it.
`SelfImage::capture` holds `/proc/self/exe` open, and the helper runs that
held image through `/proc/self/fd/<n>` with a fixed internal argument.
No path comes from UI, environment or configuration. The helper talks only
over private pipes. Before exec it joins its own process group, sets
`PR_SET_PDEATHSIG` and checks its parent; the compositor holds its pidfd.
Each request carries a nonce, request ID, generation and UID, and the UID
must equal `getuid()` (`crates/compositor/src/pam_helper.rs`,
`SelfImage::capture`, `Supervisor::spawn_inner`). The user is
`getpwuid(getuid())` (`crates/compositor/src/unlock.rs`, `session_user`).
The PAM service is `tuna-lock` unless `TUNA_PAM_SERVICE` is set
(`crates/compositor/src/pam.rs`, `service`). Evidence:
`crates/compositor/tests/pam_helper.rs`
(`private_pam_helper_actual_binary_conversation_and_retirement`, CI `check`
step); [private PAM helper](../security/private-pam-helper.md).

### IBus bridge and sandboxed clients

- The compositor inserts the IBus bridge's connection itself from a
  socketpair, with the `ime_bridge` flag (`crates/compositor/src/ime.rs`,
  `spawn`; `lib.rs`, `ClientState::ime_bridge`). Only that connection may
  bind input-method v2 and `zwp_virtual_keyboard_manager_v1` (`protocols.rs`).
  Evidence: `crates/compositor/tests/input_protocols.rs`
  (`ordinary_and_spoofed_clients_cannot_discover_or_force_bind_privileged_input_managers`,
  `a_copied_helper_on_a_public_connection_cannot_claim_input_method_authority`).
- A client accepted on a `wp_security_context_v1` listener fails every
  privileged filter, whatever its other flags, and sees neither the session
  lock nor the security-context manager (`ClientState::privileged`,
  `ClientState::sandboxed`). Evidence: `crates/compositor/tests/protocols.rs`
  (`security_context_clients_see_no_privileged_global`).

### Greeter (planned, not on `main`)

The owner added a Tuna greeter to v1 (epic #1164; PR #1166 adds it to
[ADR 0008](0008-v1-beta-track-and-release-scope.md) as item 6). It keeps GDM's daemon and replaces GDM's
GNOME Shell greeter with `tuna-compositor` in a greeter mode and a
`tuna-greeter` GTK client that uses GDM's `UserVerifier` and `Greeter`
D-Bus API. None of it ships today, and this ADR decides nothing about it.

- It runs as GDM's greeter user, not as the session user, so the
  same-UID model of this ADR does not cover it.
- It relays PAM questions and answers to GDM; GDM's root worker runs PAM.
  It holds no secrets after relaying them.
- Its identity checks are **pending implementation**: how the greeter-mode
  compositor recognises `tuna-greeter`, which privileged globals and
  interfaces the greeter may bind, and how GDM's API is reached. They must
  be recorded here, with tests, before the greeter ships (#1164).

### Summary

| Identity | Recognised by | May bind or call |
|---|---|---|
| Supervised shell | Held child PID; pidfd-pinned at Wayland accept; numeric PID on the control socket, session lock and bus | Layer-shell, foreign toplevel list, session lock, control socket, ScreenCast/Screenshot/RemoteDesktop/Introspect creation |
| Portal backend | Root-owned installed executable, plus current owner of the GNOME portal name for capture | Typed display connection with `mutter_x11_interop`; ScreenCast, Screenshot, RemoteDesktop, InputCapture, Introspect |
| gnome-remote-desktop | Root-owned installed executable plus current owner of a grd name | RemoteDesktop and its linked ScreenCast only |
| PAM helper | Spawned from the compositor's own held image over private pipes | Nothing; it only answers the compositor |
| IBus bridge | Compositor-inserted socketpair | Input-method v2, virtual keyboard |
| Greeter (planned, #1164) | Pending implementation | Pending implementation; relays PAM to GDM, holds no secrets |

## Alternatives considered

- **Inherited pre-connected sockets for the shell** (Wayland and control),
  so that no path, PID or name identifies it. This is the strongest option
  and removes PID comparison. Deferred, as in ADR 0005: it changes the
  reconnect model and every test harness.
- **pidfd identity everywhere.** The Wayland layer-shell check already pins
  the peer. The session-lock check and the D-Bus shell check could use the
  same pin (`Client` credentials captured at accept, and the bus's
  `ProcessFD` credential). This is the smallest fix for risk R2.
- **An allowlisted, root-owned shell binary** checked like the portal. It
  would stop `TUNA_SHELL_BIN` and `PATH` from choosing the shell. It does not
  stop `LD_PRELOAD` or a debugger, so it narrows R3 only.
- **Run authentication in a root service** (GDM's reauthentication, as
  GNOME does). The password would still pass through the same-UID shell.
  Kept open for the 004 production credential design.
- **Trust the session bus, as GNOME does.** Rejected as the only control:
  the identity checks cost little, refuse name claims, and keep capture
  behind consent for every caller that is not a native same-UID process.
  The out-of-scope choice is the same as GNOME's.

## Consequences

- Native same-UID processes are out of scope. A native process can obtain
  screen capture and EIS input with no consent by running the genuine
  portal or grd binary with `LD_PRELOAD` (F1). Until the indicator (F2)
  lands, such capture is also silent.
- A process that only *claims* an identity is refused: an owned portal name
  without the installed binary, a copied binary, a forged app ID, a second
  connection to the Wayland or control socket, a sandboxed client.
- Every trusted identity except the PAM helper and the IBus bridge depends
  on a PID. The shell's own PID is safe because the compositor holds it.
  The peer's PID is safe only where a pidfd pins it.
- The identity of the shell, the IBus bridge and the PAM service comes from
  the session's environment. Packaging sets none of the overrides.
- Security review of this ADR, and of [ADR 0011](0011-lock-auth-and-capture-grant-lifecycle.md),
  is what roadmap gate 4 needs before 1.0. The review's findings and their
  dispositions are listed below.

## Residual risks

These remain on `main`. The native same-UID cases are outside the boundary
above; none of them is a regression against GNOME 51. R2 and R5 are gaps
against this ADR's own checks.

- **R1. Become a trusted process (F1).** A native same-UID process can kill
  the portal or grd and start the genuine binary itself with `LD_PRELOAD`.
  `/proc/<pid>/exe` then names the genuine root-owned file, the binary
  claims its bus name, and every check passes. The preloaded code can then
  create RemoteDesktop and ScreenCast sessions and connect to EIS: screen
  capture and keystroke injection with no dialog. This needs no `ptrace`,
  and Yama `ptrace_scope` does not stop it. Where `ptrace_scope` is 0 (the
  default on Fedora up to 44 and on CentOS Stream 10), the process can also
  attach to the running shell, portal or grd. The check also reads the PID
  first and `/proc/<pid>/exe` second, without a pidfd.
- **R2. PID reuse (F4; gap).** The session-lock approval (`session_lock.rs`,
  `SessionLockHandler::lock`) and the D-Bus shell check
  (`capture_security.rs`, `authenticate_sender`, `owner_alive`) compare a
  numeric PID recorded when the peer connected. `pinned_process_id` exists
  and is not used there. An attacker that connects, hands its socket to a
  child, exits, and steers the next shell restart onto its old PID passes
  as the shell. On the bus that gives capture and RemoteDesktop input with
  no consent. On Wayland an approved lock replaces the lock owner even while
  locked (`State::resolve_lock_request`), so the attacker's lock surface
  would receive the password. [INFERENCE: not exploited in a test.]
- **R3. Choose the next session's trusted code (F9).** `TUNA_SHELL_BIN`,
  `PATH`, `TUNA_IBUS_BRIDGE_BIN` (`ime.rs`) and `TUNA_PAM_SERVICE` are read
  from the inherited environment, as are `TUNA_INTROSPECT_UNRESTRICTED`,
  `TUNA_SHELL_DBUS_UNRESTRICTED` and `LD_PRELOAD`. A same-UID process that
  edits the user's login environment (`~/.profile`, `environment.d`,
  autostart, systemd user units) controls them at the next login.
  [INFERENCE: GDM starts Wayland sessions through the login shell.] The
  same is true of `gnome-shell`. Orca, which hears lock-screen keys by
  design, also runs code from per-user customization files, as in GNOME.
- **R4. Name allowlist on `org.gnome.Shell`.** OSD, search, app-grid and
  accelerator-grab methods trust the owner of `org.gnome.Settings`,
  `org.gnome.SettingsDaemon.MediaKeys` or the portal name
  (`crates/shell-gtk/src/shell_dbus.rs`), as GNOME's `DBusSenderChecker` does.
- **R5. A stuck lock with no prompt (F3; gap).** Any unsandboxed Wayland
  client can send `ext_session_lock_v1` requests. A newer request replaces a
  pending one before the runtime decides (`SessionLockHandler::lock`), so a
  flood can make the shell's request fail. The GTK lock UI does not ask
  again while locked (`crates/shell-gtk/src/lock.rs`). The session stays
  locked with no unlock prompt until `loginctl unlock-session` or a reboot.
  It fails closed; GNOME 51 has no equivalent, because its lock is
  in-process.
- **R6. Untested paths.** No test drives the runtime's real Wayland accept
  path for the shell flag; protocol tests set it through
  `TestCompositor::add_shell_client`. The fallback without `SO_PEERPIDFD`
  (kernels before 6.5) has no test.
- **Out of scope by design:** native same-UID processes (above), root, the
  kernel, and the platform services named in the
  [security model](../security-model.md#out-of-scope).

### Review findings and dispositions

The specialist review for #1162 found no critical issue, one high (F1),
medium F2–F6 and low F7–F9. The G numbers are the gaps found while writing
these ADRs. "In progress" means a fix is being built and is not on `main`.

| Finding | Gap | Summary | Recorded in | Disposition |
|---|---|---|---|---|
| F1 (high) | — | `LD_PRELOAD` into the genuine portal or grd gives capture and EIS input with no consent | R1 | Accepted residual risk for native same-UID processes (GNOME parity); mitigations F2, grd gsettings gate and F4 in progress |
| F2 | — | No indicator or Stop for active ScreenCast, RemoteDesktop or InputCapture | ADR 0011 L13 | In progress |
| F3 | — | Foreign lock requests leave a locked session with no prompt | R5 | In progress |
| F4 | G1, G2 | Session-lock and D-Bus shell identity are unpinned PIDs | R2 | In progress |
| F5 | — | Fingerprint unlock bypasses PAM | ADR 0011 lock lifecycle, L14 | In progress: fingerprint will run through PAM |
| F6 | G3 | No logind sleep delay inhibitor | ADR 0011 L2 | In progress |
| F7 | — | A drag icon is drawn above the lock surface | ADR 0011 L11 | In progress |
| F8 | — | An unmapped idle inhibitor stops the idle lock | ADR 0011 L12 | In progress |
| F9 | G4, G5 | Environment overrides; `loginctl unlock-session`; Orca customization | R3, ADR 0011 L1 | Accepted residual risk (GNOME parity) |
| — | G6 | The portal can call `org.tuna.Screenshot.ScreenshotWindows` | ADR 0011 L8 | In progress |
| — | G7 | Security lanes are not required per pull request | ADR 0011 L10 | Accepted residual risk; they run on `main` and nightly |
| — | G8 | P-LK-01 cites a test of a non-production unlock path | ADR 0011 L10 | In progress |
| — | G9 | Untested: delivered EIS revocation, the real accept path, the no-pidfd fallback | R6, ADR 0011 L7 | In progress |

The F9 note that `docs/security-model.md` called input-method v2 open to
any client is fixed there.

### Comparison with GNOME 51

Sources: Mutter 51.1 at
[`9134e3a`](https://github.com/GNOME/mutter/tree/9134e3a5869ce232bf95b77519005865747ba603)
and GNOME Shell 51.1 at
[`33da068`](https://github.com/GNOME/gnome-shell/tree/33da06854e229c256afd5872fedd83c28cbfa176).

| Concern | GNOME 51 | Tuna Desktop today |
|---|---|---|
| Who may create ScreenCast and RemoteDesktop sessions | Any session-bus caller; Mutter records the caller and checks nothing else ([`meta-dbus-session-manager.c`](https://github.com/GNOME/mutter/blob/9134e3a5869ce232bf95b77519005865747ba603/src/backends/meta-dbus-session-manager.c)). Maintainers state same-user bus access is the boundary ([GNOME Discourse 35009](https://discourse.gnome.org/t/security-for-mutter-screencast/35009)). | Shell PID, or portal name plus installed executable, or grd name plus installed executable |
| Portal backend's display connection | `ServiceChannel` accepts any caller by PID and replaces the previous client of that type ([`meta-service-channel.c`](https://github.com/GNOME/mutter/blob/9134e3a5869ce232bf95b77519005865747ba603/src/core/meta-service-channel.c)) | Installed root-owned executable for the requested type |
| Screenshot and Introspect | Name allowlist (`DBusSenderChecker`, [`util.js`](https://github.com/GNOME/gnome-shell/blob/33da06854e229c256afd5872fedd83c28cbfa176/js/misc/util.js)), skipped in unsafe mode | Shell PID or authenticated portal |
| Shell and lock identity | In-process with the compositor; no IPC identity needed | Separate child; PID identity (R2) |
| Privileged Wayland globals | No layer-shell, session lock, virtual keyboard or foreign toplevel list at all | Offered, filtered to the shell or IBus bridge |
| `ptrace`, `LD_PRELOAD`, login environment | Defeat it (R1, R3) | Defeat it (R1, R3) |
| Lock authentication | GDM's root session worker, `gdm-password` | User-run private helper, `tuna-lock` (ADR 0011) |

The [xdg-desktop-portal security model](https://flatpak.github.io/xdg-desktop-portal/docs/_sources/security-model.rst.txt)
states that the desktop session is trusted and only sandboxed applications
are not. Tuna Desktop adds identity checks against same-UID name claims.
It makes no stronger claim than that.

## Evidence

- Unit and integration tests named above, run by the CI `check` job
  (`cargo test --workspace` and its named steps in `.github/workflows/ci.yml`).
- CI `portal-security` and `remote-security`, against the installed GNOME
  portal stack ([capture-security-evidence.json](../capture-security-evidence.json)).
  They are rotated lanes: a pull request that changes only
  `crates/compositor` runs them only when the rotation picks them
  (`scripts/ci-select-lanes`, `ROTATION`). They always run on `main`.
