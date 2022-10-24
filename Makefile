
install_dev:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch

run:
	CARGO_NET_GIT_FETCH_WITH_CLI=true @echo cargo run 'libc' Cargo.toml

build:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build

test:
	#yes "Some abcdef text" | head -n 100000 > large-file.txt
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo run 'libc' Cargo.toml
	#rm large-file.txt

install:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --path .
