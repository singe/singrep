SHELL=/bin/bash
%:
    @:

args = `arg="$(filter-out $@,$(MAKECMDGOALS))" && echo $${arg:-${1}}`

install_dev:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch

#TASKS = \
#    run
#
.PHONY: run
run:
	@cargo run $(call args,defaultstring)

build:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --release

test:
	#yes "Some abcdef text" | head -n 100000 > large-file.txt
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo run 'libc' Cargo.toml
	#rm large-file.txt

install:
	CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --path .

clean:
	cargo clean
