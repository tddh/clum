# clum 项目开发规则

## MCP 工具使用规则（最高优先级）

**以下规则优先级最高，必须严格遵守：**

1. **默认会话**：所有 clum 操作必须使用 `session_name="clum"`，除非用户明确指定其他会话名
2. **默认 Pane**：`pane_id` 可省略，server 自动选择 window 0 中编号最小的 pane。破坏性工具（`close_pane`、`paste_buffer`、`respawn_pane`）必须显式指定
3. **禁止随意创建会话**：不要自作主张创建 `test-session`、`debug-session` 等新会话，除非用户明确要求
4. **先 attach 后 create**：操作前先 `session_attach` 检查会话是否存在，不存在才 `session_create`
5. **保留会话**：执行完命令后，不要主动清理 session（禁止调用 `kill_session`、`close_window`、`close_pane`），除非用户明确要求"清理"、"关闭"、"销毁"

6. **以用户指令为主**：用户的明确指令优先于以上所有默认规则。如果用户指令信息不明确（如未指定主机、会话名、操作目标等），必须先向用户确认再执行，禁止猜测或自作主张
7. **开发/测试隔离**：在 clum 项目自身开发调试期间，若需在远程主机上执行测试命令（如重启 bridge、修改配置、编译验证等），必须创建独立的临时会话（如 `session_name="dev"`），严禁占用 `clum` 默认会话。测试完成后清理该会话（`kill_session`）。**严禁对 `clum` 默认会话执行 `exit`、`kill_session`、`close_pane`、`close_window` 等破坏性操作**——即使用户明确指示了相关操作，也必须先确认目标是否为 `clum` 会话，若是则拒绝执行并说明原因。

**违反这些规则 = BUG，没有例外。**

## 开发规范

- Rust 代码必须通过 `cargo clippy --workspace -- -D warnings` 无警告
- 提交前运行 `just check` 确保编译通过
- 遵循现有代码风格和命名规范
- 新功能必须添加对应的测试
- 文档更新与代码变更同步

## 项目结构

```
clum/
├── crates/              # Rust crates
│   ├── clum-cli/   # CLI 交互式终端
│   ├── clum-core/  # 共享类型（HostConfig, AuditEvent, AuditAction）
│   ├── clum-mcp/   # MCP Server（Central Server 模式 / stdio 本地模式）
│   └── rmux-bridge/   # Bridge proxy
├── config/              # 配置文件
├── docs/                # 文档
├── deploy/              # 部署脚本（install.sh, deploy-bridge.sh, deploy-mcp.sh）
├── scripts/             # 迁移与测试脚本（migrate-to-clum.sh, mcp_smoke.py）
├── .opencode/skills/    # AI 开发辅助 Skills (OpenCode)
└── .qoder/skills/       # AI 开发辅助 Skills (Qoder)
```

## 常用命令

```bash
just check       # cargo check --workspace
just test        # cargo test --workspace
just fmt         # cargo fmt --all
just lint        # cargo clippy --workspace -- -D warnings
just build       # cargo build --workspace
just release-linux  # 交叉编译 Linux x86_64
```
