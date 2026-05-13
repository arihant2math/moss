.globl _start
.section ".text.boot"

.intel_syntax noprefix
.code64

# Entry point for the x86_64 kernel image.
#
# This mirrors the shape of the arm64 bootstrap stub:
#   1. preserve any loader-provided arguments
#   2. zero the .bss section
#   3. install a temporary boot stack
#   4. hand off to Rust stage-1 / stage-2 boot code
#
# The x86_64 port does not yet have a real exception-return path, so the boot
# code parks the CPU once the Rust handoff returns.
_start:
    mov     r12, rdi
    mov     r13, rsi

    cli
    cld
    xor     ebp, ebp

    lea     rdi, [rip + __bss_start]
    lea     rcx, [rip + __bss_end]
    sub     rcx, rdi
    xor     eax, eax
    shr     rcx, 3
    rep     stosq

    lea     rsp, [rip + __boot_stack]
    and     rsp, -16

    mov     rdi, r12
    mov     rsi, r13
    lea     rdx, [rip + __image_start]
    lea     rcx, [rip + __image_end]
    lea     r8, [rip + __init_pages_start]
    call    arch_init_stage1

    mov     rsp, rax
    and     rsp, -16

    # Reserve a small bootstrap frame until the real x86_64 exception/context
    # layout exists.
    sub     rsp, 128
    mov     rdi, rsp
    call    arch_init_stage2

    jmp     park_cpu
