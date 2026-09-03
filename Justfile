#!/usr/bin/env just --justfile

create-image:
    ./scripts/create-image.sh

create-initramfs:
    #!/usr/bin/env sh
    if [ ! -f build/rootfs ]; then
    just create-image
    fi
    ./scripts/create-initramfs.sh

run:
    #!/usr/bin/env sh
    # Create moss.img if it doesn't exist
    if [ ! -f moss.img ]; then
    just create-image
    fi
    cargo run --release -- --init /bin/ash

test-unit:
    #!/usr/bin/env sh
    host_target="$(rustc --version --verbose | awk -F': ' '/^host:/ {print $2; exit}')"
    cargo test --package libkernel --target "$host_target"

test-kunit:
    cargo test --release

test-userspace:
    cargo run -r -- --init /bin/usertest
