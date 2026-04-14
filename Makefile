.PHONY: all setup check lint fix build test clean

all: check lint build

setup:
	rustup update
	rustup component add clippy rustfmt

check:
	@echo "🔍 Checking code for compilation errors..."
	cargo check

fix:
	@echo "🔧 Auto-fixing issues..."
	cargo clippy --fix --allow-dirty -- -D warnings
	cargo fix --allow-dirty
	find . -path ./target -prune -o -name "*.rs" -exec sed -i 's/[[:space:]]*$$//' {} +
	cargo fmt

lint:
	@echo "🧹 Running linter and formatter (check mode)..."
	cargo fmt -- --check
	cargo clippy -- -D warnings

build:
	@echo "🏗️ Building release version..."
	cargo build --release

test:
	@echo "🧪 Running tests..."
	cargo test

clean:
	@echo "🗑️ Cleaning artifacts..."
	cargo clean

run:
	@echo "🚀 Running the application..."
	cargo run