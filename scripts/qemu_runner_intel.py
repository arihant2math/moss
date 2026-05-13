#!/usr/bin/env python3

import argparse
import subprocess
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description="QEMU x86_64 runner")

parser.add_argument("elf_executable", help="Location of compiled kernel image to run")
parser.add_argument("--init", default="/bin/sh", help="Location of the init process (in the rootfs)")
parser.add_argument("--rootfs", default="moss.img", help="Location of the root filesystem image to use")
parser.add_argument("--smp", default=4, help="Number of CPU cores to use")
parser.add_argument("--memory", default="2G")
parser.add_argument("--debug", action="store_true", help="Enable QEMU debugging")

args = parser.parse_args()
kernel_path = Path(args.elf_executable)


def is_elf(path: Path) -> bool:
    with path.open("rb") as f:
        return f.read(4) == b"\x7fELF"


if args.init.split("/")[-1] in ["bash", "sh"]:
    append_args = f"--init={args.init} --init-arg=-i"
else:
    append_args = f"--init={args.init}"

boot_args = (
    f"{append_args} --rootfs=ext4fs --automount=/dev,devfs "
    "--automount=/tmp,tmpfs --automount=/proc,procfs --automount=/sys,sysfs"
)

qemu_command = [
    "qemu-system-x86_64",
    "-M",
    "microvm,rtc=on,isa-serial=on",
    "-cpu",
    "max",
    "-m",
    args.memory,
    "-smp",
    str(args.smp),
    "-rtc",
    "base=utc,clock=host",
    "-nographic",
    "-serial",
    "stdio",
    "-monitor",
    "none",
]

if is_elf(kernel_path):
    # On x86 QEMU's -kernel path expects a Linux bzImage or an ELF with a PVH
    # note. Moss currently produces a plain ELF, so load it through the generic
    # loader instead of direct Linux boot.
    qemu_command += ["-device", f"loader,file={kernel_path},cpu-num=0"]

    if args.init != parser.get_default("init") or args.rootfs != parser.get_default("rootfs"):
        print(
            "warning: ignoring --init/--rootfs for x86_64 ELF boot; "
            "QEMU only supports -append/-initrd with -kernel bzImage/PVH boot",
            file=sys.stderr,
        )
else:
    qemu_command += [
        "-kernel",
        str(kernel_path),
        "-initrd",
        args.rootfs,
        "-append",
        boot_args,
    ]

qemu_command += ["-device", "virtio-rng-device"]

if args.debug:
    qemu_command += ["-s", "-S"]

subprocess.run(qemu_command, check=True)
