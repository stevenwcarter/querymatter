
install:
  cargo install --path .

mold-install:
  mold -run cargo install --path .

samples:
  cargo run --bin querymatter-samples -- --force --scale 1k samples
