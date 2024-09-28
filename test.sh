#!/bin/bash

set -e

cargo build --release

echo ''

set -x

./target/release/dsim /dev/null -p list -vvv

./target/release/dsim example-data/simcontrol.toml -n 128 -p a770 -vvv

./target/release/dsim example-data/simcontrol.toml -n 128 -p intel -vvv


