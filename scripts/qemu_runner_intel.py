#!/usr/bin/env python3

import argparse
import subprocess

parser = argparse.ArgumentParser(description="QEMU x86_64 runner")

parser.add_argument("elf_executable", help="Location of compiled ELF executable to run")
parser.add_argument("--init", default="/bin/sh", help="Location of the init process (in the rootfs)")
parser.add_argument("--rootfs", default="moss.img", help="Location of the root filesystem image to use")
parser.add_argument("--smp", default=4, help="Number of CPU cores to use")
parser.add_argument("--memory", default="2G")
parser.add_argument("--debug", action="store_true", help="Enable QEMU debugging")

args = parser.parse_args()

if args.init.split("/")[-1] in ["bash", "sh"]:
    append_args = f"--init={args.init} --init-arg=-i"
else:
    append_args = f"--init={args.init}"

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
    "-kernel",
    args.elf_executable,
    "-initrd",
    args.rootfs,
    "-append",
    f"{append_args} --rootfs=ext4fs --automount=/dev,devfs --automount=/tmp,tmpfs --automount=/proc,procfs --automount=/sys,sysfs",
    "-device",
    "virtio-rng-device",
]

if args.debug:
    qemu_command += ["-s", "-S"]

subprocess.run(qemu_command, check=True)
