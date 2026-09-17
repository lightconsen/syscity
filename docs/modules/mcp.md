# MCP Module

Model Context Protocol integration for connecting to external MCP servers and using their tools.

## Design

MCP is an open protocol for extending LLM capabilities through external servers. Syscity implements an MCP client that discovers tools from servers and registers them dynamically in the `ToolRegistry`.

- **`McpClient`** — Per-server JSON-RPC 2.0 client supporting multiple transports
- **`McpManager`** — Manages multiple `McpClient` instances, handles connect/disconnect/reconnect
- **`McpToolDefinition`** — Tool schema discovered from `tools/list`
- **`McpToolWrapper`** — Wraps an MCP tool as a Syscity `Tool` trait object for `ToolRegistry`
- **`McpSettings`** / **`McpServerConfig`** — Configuration in `syscity.toml` `[mcp.servers.*]`

### Supported Transports

| Transport | Use Case |
|-----------|----------|
| `stdio` | Spawn a local subprocess (default) |
| `sse` | Connect to an HTTP server via Server-Sent Events |
| `streamable_http` | POST requests with SSE response bodies |

### Protocol Flow

1. `connect_stdio()` or `connect_sse()` establishes transport
2. `initialize()` sends `initialize` request, receives server info
3. `tools/list` discovers available tools
4. `McpToolWrapper` registers each tool into `ToolRegistry` via `register_dynamic()`
5. On disconnect, `deregister_prefix()` cleans up `mcp__{server}__*` tools

### Configuration Example

```toml
[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/allowed"]
auto_connect = true

[mcp.servers.fetch]
transport = "sse"
url = "http://localhost:3000/sse"
```

### Resource Support

- `resources/list` — Discover available resources (files, APIs, databases)
- `resources/read` — Read resource content by URI
- Environment variable resolution (`$VAR` → resolved at connect time)

## Key Types

```rust
pub struct McpClient {
    process: Option<Child>,
    request_tx: Option<mpsc::UnboundedSender<McpRequest>>,
    server_info: Option<McpServerInfo>,
    tools: Vec<McpToolDefinition>,
    timeout_secs: u64,
}

pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, Arc<RwLock<McpClient>>>>>,
}

pub struct McpServerConfig {
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
    pub timeout_secs: u64,
    pub auto_connect: bool,
}
```

## Implemented Features

- Multi-transport MCP client (stdio, SSE, streamable HTTP)
- JSON-RPC 2.0 protocol implementation
- Auto-discovery and registration of MCP tools
- Prefix-based tool cleanup on disconnect
- Resource discovery and reading
- Environment variable resolution in server configs
- Timeout and auto-connect configuration
- Integration with `ToolRegistry` for dynamic tool registration


## Tool Call Authorization

MCP tools are registered as `mcp__{server}__{tool}`, and both the agent path and
the `mcp.call_tool` WS method reach them **through the `ToolRegistry`**
(`execute_call`). That is what makes blocked and degraded prefixes, policy hooks,
the approval route and the content filter apply to a call however it was
started — calling the MCP client directly, which the WS method used to do,
bypassed all of them.

Two details of that path are worth knowing:

- The context `mcp.call_tool` builds carries **no ask channel**, so a tool that
  merely advertises `requires_approval` does not stop to ask a human: the caller
  is the operator who asked for it. A policy *hook* that returns
  `NeedsApproval` is still honoured, and routed through the full approval flow.
- A name the registry cannot dispatch — a server that is not connected, or one
  exposing more tools than `max_tools` registered — falls back to a direct client
  call, because the rest of the policy needs a registry entry to evaluate. The
  blocked-name list still applies there; nothing else does.
