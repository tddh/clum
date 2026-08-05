# clum build commands

default: check

# ─── 编译 ────────────────────────────
check:
    cargo check --workspace

build:
    cargo build --workspace

release:
    cargo build --workspace --release

# 交叉编译 Linux x86_64
release-linux:
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc cargo build --target x86_64-unknown-linux-musl --release -p rmux-bridge
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc cargo build --target x86_64-unknown-linux-musl --release -p clum-mcp

# 交叉编译 Windows x86_64（MCP 客户端）
release-windows:
    cargo build --target x86_64-pc-windows-msvc --release -p clum-mcp -p clum-cli

check-bridge:
    cargo check -p rmux-bridge

check-mcp:
    cargo check -p clum-mcp

build-bridge:
    cargo build -p rmux-bridge --release

build-mcp:
    cargo build -p clum-mcp --release

# ─── 测试 ────────────────────────────
test:
    cargo test --workspace

# ─── 代码质量 ────────────────────────
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace -- -D warnings

# ─── 清理 ────────────────────────────
clean:
    cargo clean

# ─── 证书 ────────────────────────────
# 生成 CA 根证书（只需一次）
certs:
    bash deploy/generate-certs.sh certs

# 为主机签发独立证书
certs-host host:
    bash deploy/generate-certs.sh certs {{host}}

# ─── 部署 ────────────────────────────

# 部署 bridge（首次或重新配置）mode=hub|direct
deploy-bridge host mode='hub':
    BRIDGE_TOKEN="${BRIDGE_TOKEN:?set BRIDGE_TOKEN env var}" bash deploy/deploy-bridge.sh ./target/x86_64-unknown-linux-musl/release/rmux-bridge {{host}} {{mode}}

# 更新 bridge 二进制（不动配置）
update-bridge host:
    bash deploy/update-bridge.sh ./target/x86_64-unknown-linux-musl/release/rmux-bridge {{host}}

# (deprecated) 旧部署脚本，使用 deploy-bridge 代替
deploy host token='{{token}}':
    bash deploy/generate-certs.sh certs $(echo {{host}} | sed 's/.*@//' | cut -d: -f1)
    BRIDGE_TOKEN="{{token}}" bash deploy/install-bridge.sh ./target/x86_64-unknown-linux-musl/release/rmux-bridge {{host}} certs

# ─── 推送 ────────────────────────────
push-all:
    git push origin master
    #git push gitlab master
    git push gitee master
