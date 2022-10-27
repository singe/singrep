# singrep
singe's grep - a fast grep using single-file parallelism

singrep makes use of deterministic kernel file cache'ing to read the file fast enough to make multi-threading useful. It instructs the kernel to cache sections of the file to memory, then memory maps them for fast reads. Chunks of the file are then sent to separate thread to do the matching. On a modern multi-core system, this is significantly faster than other fast grep utilities.

This only works on Linux and macOS.

# Installation
## Install to your cargo bin 
Command installs binary to /Users/<user>/.cargo/bin/singrep
As long as you have done the following: `source $HOME/.cargo/env`
```commandline
make install

# or 

CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --path .
```

## Install development depedencies
```commandline
make install_dev
```
# Build binary

## Build binary
```commandline
make build

# or 

CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build
```

# Usage

`singrep <pattern> <file>`

Will search for occurances of *pattern* in the supplied *file*.

# Advanced usage

* Exact Match --exact, -e - will only match lines that entirely match the pattern
* First Match --first, -f - will exit after the first match is found
* Byte Position --position, -p - will display the *byte (not line) number* where the pattern was found
* Verbose --verbose, -v - will display some extra information

# Performance Tuning

## Block Size --block, -b

The block size controls how big a block will be read from the file at a time. This depends on the optimal speed of your drive. By default it is 8M (8_388_608). One way to test this is to do the following on a large file:

`for x in 1M 1M 2M 4M 8M 12M; do time dd if=somefile of=/dev/null bs=$x; done`

Running in `--verbose` mode will give stats on how fast the file was read from disk, for optimisation.

## Cache Size --cache, -c

The cache size control how big the blocks of the file that are cached to the kernel's file pages are. On the systems I tested, this is about 68% of total system memory. But, if there's a ton of stuff running, your file cache can have less available space (MS Teams is a great way to test this). By default it is set to 2G (2_147_483_648).

You can find total memory with:

Linux
`cat /proc/meminfo |head -n1`

macOS
`sysctl hw.memsize`

## Shard Size --shard, -s

The shard size controls how big the blocks of data to send to the threads should be. Running with `--verbose` and examining the thread waits can help to optimise this for your system. Fewer waits means the threads spend less time waiting for a new chunk to arrive.
