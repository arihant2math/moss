#!/usr/bin/env bash
set -e

# First argument: the ELF file to run (required)
# All subsequent arguments can override defaults passed to qemu-system-aarch64 or be forwarded as-is.
# Usage: ./qemu-runner.sh <path/to/kernel.elf> [qemu or append args]

# Parse args:
if [ $# -lt 1 ]; then
    echo "Usage: $0 <elf-file> [qemu/append args]"
    exit 1
fi

base="$( cd "$( dirname "${BASH_SOURCE[0]}" )"/.. && pwd )"

elf="$1"
# Lose first argument
shift
bin="${elf%.elf}.bin"

# Defaults (can be overridden by subsequent args)
M_OPT="virt,gic-version=3"
INITRD_OPT="moss.img"
CPU_OPT="cortex-a72"
MEM_OPT="2G"
SMP_OPT="4"
NOGRAPHIC_OPT=1      # 1 => include -nographic, 0 => omit
S_FLAG_OPT=1         # 1 => include -s, 0 => omit

# Default kernel command line (append)
APPEND_OPTS=(
  "--init=/bin/bash"
  "--init-arg=-i"
  "--rootfs=ext4fs"
  "--automount=/dev,devfs"
  "--automount=/tmp,tmpfs"
  "--automount=/proc,procfs"
)

# Extra QEMU options that are not recognized explicitly will be forwarded
EXTRA_QEMU_OPTS=()

# Push or override specific append sub-args (like --init=...)
# This will replace existing entry if key matches, else append a new one.
function set_append_kv() {
  local key="$1"; shift
  local value="$1"; shift || true
  local new_entry
  if [[ -n "$value" ]]; then
    new_entry="${key}=${value}"
  else
    new_entry="${key}"
  fi
  local found=0
  for i in "${!APPEND_OPTS[@]}"; do
    if [[ "${APPEND_OPTS[$i]}" == ${key}=* || "${APPEND_OPTS[$i]}" == "${key}" ]]; then
      APPEND_OPTS[$i]="$new_entry"
      found=1
      break
    fi
  done
  if [[ $found -eq 0 ]]; then
    APPEND_OPTS+=("$new_entry")
  fi
}

# Parse user-provided overrides / forwards
while (( "$#" )); do
  case "$1" in
    -M)
      shift; M_OPT="${1}" ;;
    -M=*)
      M_OPT="${1#-M=}" ;;

    -initrd)
      shift; INITRD_OPT="${1}" ;;
    -initrd=*)
      INITRD_OPT="${1#-initrd=}" ;;

    -cpu)
      shift; CPU_OPT="${1}" ;;
    -cpu=*)
      CPU_OPT="${1#-cpu=}" ;;

    -m)
      shift; MEM_OPT="${1}" ;;
    -m=*)
      MEM_OPT="${1#-m=}" ;;

    -smp)
      shift; SMP_OPT="${1}" ;;
    -smp=*)
      SMP_OPT="${1#-smp=}" ;;

    -nographic)
      NOGRAPHIC_OPT=1 ;;
    -display*|--display*)
      # -nographic not compatible with display
      NOGRAPHIC_OPT=0
      EXTRA_QEMU_OPTS+=("$1") ;;

    -s)
      S_FLAG_OPT=1 ;;
    -S)
      # Pauses CPU at startup, include -S and still allow -s unless overridden
      EXTRA_QEMU_OPTS+=("-S") ;;

    -append)
      # override APPEND_OPTS entirely
      shift
      APPEND_OPTS=("${1}") ;;
    -append=*)
      APPEND_OPTS=("${1#-append=}") ;;

    --init=*)
      set_append_kv "--init" "${1#--init=}" ;;
    --init-arg=*)
      set_append_kv "--init-arg" "${1#--init-arg=}" ;;
    --rootfs=*)
      set_append_kv "--rootfs" "${1#--rootfs=}" ;;
    --automount=*)
      set_append_kv "--automount" "${1#--automount=}" ;;

    # Unknown or additional options: forward as-is
    *)
      EXTRA_QEMU_OPTS+=("$1") ;;
  esac
  shift || true
done

# Convert to binary format
aarch64-none-elf-objcopy -O binary "$elf" "$bin"

# Construct final -append string
# If APPEND_OPTS contains a single string because of -append override, use it directly.
if [[ ${#APPEND_OPTS[@]} -eq 1 ]]; then
  FINAL_APPEND="${APPEND_OPTS[0]}"
else
  FINAL_APPEND="${APPEND_OPTS[*]}"
fi

# Build the QEMU command
CMD=(
  qemu-system-aarch64
  -M "$M_OPT"
  -initrd "$INITRD_OPT"
  -cpu "$CPU_OPT"
  -m "$MEM_OPT"
  -smp "$SMP_OPT"
)

if [[ $NOGRAPHIC_OPT -eq 1 ]]; then
  CMD+=( -nographic )
fi
if [[ $S_FLAG_OPT -eq 1 ]]; then
  CMD+=( -s )
fi

CMD+=( -kernel "$bin" -append "$FINAL_APPEND" )

# Add any extra forwarded QEMU options
if [[ ${#EXTRA_QEMU_OPTS[@]} -gt 0 ]]; then
  CMD+=( "${EXTRA_QEMU_OPTS[@]}" )
fi

# Execute
"${CMD[@]}"
