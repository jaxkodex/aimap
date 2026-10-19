# Toolchain environment for this sandbox (no system gcc; rust-lld + zig).
# Source before cargo commands:  . ../rust/env.sh
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTUP_TOOLCHAIN=stable-x86_64-unknown-linux-gnu
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$HOME/.cargo/bin/zig-cc-gnu"
export TEST_DATABASE_URL="${TEST_DATABASE_URL:-postgresql://postgres@127.0.0.1:5433/postgres}"
