# Capabilities Module

Platform capability sets for Syscity — organizes platform-specific tools by environment.

## Design

`PlatformToolSet` (in `src/computer/platform/`) is a way to group platform-specific tools by environment (Linux Server, macOS Desktop, etc.). Tools are registered individually into `ToolRegistry`; `PlatformToolSet` is only an organizational unit that controls *which* tools are exposed for the current environment.

- **`PlatformToolSet` trait** — Defines a set of tools scoped to a specific platform/environment
- **`CapabilityProfile`** — Pre-defined profiles: Minimal, Observer, Server, Desktop, Full, Custom
- **`PlatformCapabilityRegistry`** (`platform/registry.rs`) — Holds all registered capability sets and manages enablement
- **`PlatformConstraints`** — Runtime availability checks (target OS, GUI requirement, services)
- **`OsControlScope`** — Permission scope hierarchy: ReadOnly, UserSpace, System, Root

### Platform Adapters

| Platform | Module | Description |
|----------|--------|-------------|
| Linux Server | `platform/linux/` | Server-oriented tools (systemd, logs, processes, packages, firewall) |
| Linux Desktop (Wayland) | `platform/wayland/` | Wayland desktop tools |
| Linux Desktop (X11) | `platform/x11/` | X11 desktop tools |
| macOS | `platform/macos/` | macOS-specific desktop automation |
| Windows | `platform/windows/` | Windows-specific desktop automation |
| Android | `platform/android/` | Android devices over ADB |
| iOS | `platform/ios/` | iOS devices over libimobiledevice |

## Key Types

```rust
pub trait PlatformToolSet: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn constraints(&self) -> &PlatformConstraints;
    fn scope(&self) -> OsControlScope;
    fn tools(&self) -> Vec<Box<dyn Tool>>;
    fn is_available(&self) -> bool;
}

pub enum CapabilityProfile {
    Minimal,    // Only built-in generic tools
    Observer,   // Read-only observation only
    Server,     // Server-oriented sets (no GUI required)
    Desktop,    // Desktop-oriented sets (GUI required)
    Full,       // All available sets
    Custom(Vec<String>), // Explicit list of set IDs
}

pub enum OsControlScope {
    ReadOnly = 0,
    UserSpace = 1,
    System = 2,
    Root = 3,
}

pub struct PlatformConstraints {
    pub target_os: Vec<String>,
    pub requires_gui: bool,
    pub requires_services: Vec<String>,
}
```

## Data Flow

```
Config::capabilities.profile
    │
    ▼
CapabilityProfile::apply(registry)
    │
    ├──▶ Minimal → disable all OS-specific sets
    ├──▶ Observer → disable non-read-only sets
    ├──▶ Server → disable GUI-requiring sets
    ├──▶ Desktop → disable non-GUI sets
    ├──▶ Full → enable all available sets
    └──▶ Custom → enable only specified sets
            │
            ▼
        PlatformCapabilityRegistry::export_to_tool_registry()
```

## Implemented Features

- Platform constraint detection (OS, display server, services)
- Capability profile system with 5 pre-defined profiles + custom
- Permission scope hierarchy (ReadOnly → UserSpace → System → Root)
- Tool conflict resolution strategies (Reject, Override)
- Display server detection (X11, Wayland, macOS, Windows)
- Service availability checks (systemd, etc.)
- Config-driven capability selection via `CapabilitiesConfig`

