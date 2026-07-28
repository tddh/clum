# yunying 项目开发规则

## MCP 工具使用规则（最高优先级）

**以下规则优先级最高，必须严格遵守：**

1. **默认会话**：所有 yunying 操作必须使用 `session_name="yunying"`，除非用户明确指定其他会话名
2. **默认 Pane**：`pane_id` 可省略，server 自动选择 window 0 中编号最小的 pane。破坏性工具（`close_pane`、`paste_buffer`、`respawn_pane`）必须显式指定
3. **禁止随意创建会话**：不要自作主张创建 `test-session`、`debug-session` 等新会话，除非用户明确要求
4. **先 attach 后 create**：操作前先 `session_attach` 检查会话是否存在，不存在才 `session_create`
5. **保留会话**：执行完命令后，不要主动清理 session（禁止调用 `kill_session`、`close_window`、`close_pane`），除非用户明确要求"清理"、"关闭"、"销毁"

6. **以用户指令为主**：用户的明确指令优先于以上所有默认规则。如果用户指令信息不明确（如未指定主机、会话名、操作目标等），必须先向用户确认再执行，禁止猜测或自作主张

**违反这些规则 = BUG，没有例外。**

## 开发规范

- Rust 代码必须通过 `cargo clippy --workspace -- -D warnings` 无警告
- 提交前运行 `just check` 确保编译通过
- 遵循现有代码风格和命名规范
- 新功能必须添加对应的测试
- 文档更新与代码变更同步

## 项目结构

```
yunying/
├── crates/              # Rust crates
│   ├── yunying-cli/   # CLI 交互式终端
│   ├── yunying-core/  # 共享类型（HostConfig, AuditEvent, AuditAction）
│   ├── yunying-mcp/   # MCP Server
│   └── rmux-bridge/     # Bridge proxy
├── config/              # 配置文件
├── docs/                # 文档
├── deploy/              # 部署脚本
└── .opencode/skills/    # AI 开发辅助 Skills
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
