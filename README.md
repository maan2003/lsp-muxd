# LSP Multiplexer Daemon (lsp-muxd)

A lightweight daemon that enables a single Language Server Protocol (LSP) instance to efficiently serve multiple workspaces simultaneously. Instead of spawning separate language servers for each project or workspace, lsp-muxd leverages the LSP "workspaceFolders" specification to dynamically add and remove project folders on the same server instance.

## Usecase

I want to have 5 git worktrees for LLMs and use LSP on them. This lets me do that on a 8GB ram machine.

## Key Benefits

- **Resource Efficient**: Significantly less CPU and memory overhead for each additional workspace compared to running separate language server instances
- **Seamless Integration**: Transparently handles multiple worktrees or projects through a single LSP server

## Usage

```bash
lsp-muxd [--instance-id <ID>] <server-command> [server-args]...
```

The daemon automatically manages workspace additions and removals as clients connect and disconnect, ensuring optimal resource utilization while maintaining full LSP functionality.
