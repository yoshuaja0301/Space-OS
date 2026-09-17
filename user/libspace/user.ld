/* Space OS user program: static ELF64 loaded at 4 MiB. */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data   PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(4K) { *(.text .text.*) } :text
    .rodata : ALIGN(4K) { *(.rodata .rodata.*) } :rodata
    .data : ALIGN(4K) { *(.data .data.*) } :data
    .bss : ALIGN(4K) { *(.bss .bss.*) *(COMMON) . = ALIGN(4K); } :data

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr) *(.note .note.*) *(.comment) }
}
