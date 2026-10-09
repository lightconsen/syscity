# base64-tool — a Syscity WASM plugin

One tool, `base64`, with `mode = "encode" | "decode"`. Pure computation: it
imports no host functions and declares no permissions, so it cannot reach the
filesystem, the network, or the agent's memory.

This is the reference example for the plugin ABI — the one thing the other
example in this directory is not (see the note at the end).

## Build

```bash
rustup target add wasm32-unknown-unknown   # once
cargo build --release --target wasm32-unknown-unknown
```

The artifact is `target/wasm32-unknown-unknown/release/base64_tool.wasm`
(~85 KB).

## Test it

The ABI is exercised by
`src/plugins/mod.rs::tests::a_real_wasm_plugin_answers_a_tool_call`, which loads
the built module through `PluginManager` and calls the tool. It reads the
artifact from `tests/fixtures/base64-tool.wasm`, so after changing anything
here:

```bash
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/base64_tool.wasm \
   ../../../tests/fixtures/base64-tool.wasm
```

The fixture is committed because rustc cannot produce a wasm at test time. It
can drift from this source if that copy step is skipped — the functional test
catches a wrong module, but only a rebuild catches a stale one.

## Install it locally

```bash
# A plugin is a directory: the manifest plus the wasm it names.
mkdir -p ~/.syscity/plugins/base64-tool
cp plugin.json base64-tool.wasm ~/.syscity/plugins/base64-tool/
syscity plugin reload
```

Or from the marketplace, once this entry is published:
`syscity plugin catalog-install base64-tool`.

## The ABI, as this plugin implements it

Defined by `src/plugins/runtime/mod.rs::invoke_wasm_tool`. The host:

1. resolves the guest's `memory` export and its `alloc(i32) -> i32` export;
2. calls `alloc` **three times in a fixed order** — tool name, JSON params, then
   the output buffer — writing into each;
3. calls `call_tool(name_ptr, name_len, params_ptr, params_len, out_ptr,
   out_max) -> i32`, whose return value is the number of bytes written to
   `out_ptr`, or negative for an error;
4. reads that many bytes as UTF-8 and parses them as JSON.

There is no `free` export and nothing marks where one invocation ends and the
next begins. That is why `alloc` here hands back one of three reserved buffers,
cycled: a normal allocator would leak once per call for the lifetime of the
instance. The cycle is valid because the host asks for the same three things in
the same order every time; if the host's order ever changed, the worst case is a
call failing visibly, not memory corruption.

A guest may also export a per-tool function named after the tool
(`(params_ptr, params_len, out_ptr, out_max) -> i32`) instead of the generic
`call_tool`; the host prefers `call_tool` when both exist.

## Note on the other example

`examples/wasm-plugins/echo-channel` is **stale**: it targets the
`wit/channel.wit` component-model world and ships a YAML manifest, while the
current loader reads a core wasm module with the ABI above and a `plugin.json`
in `PluginManifest` form. It is not a working example of this system.
