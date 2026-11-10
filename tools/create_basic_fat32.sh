#!/bin/sh
# 在同目录临时文件中生成并检查镜像，成功后再替换输出。
set -eu

out=${1:?usage: create_basic_fat32.sh OUTPUT}
mkdir -p "$(dirname -- "$out")"
work=$(mktemp -d "$(dirname -- "$out")/.fat32.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

truncate -s 64M "$work/basic-fat32.img"
mkfs.fat -F 32 -S 512 -s 1 -i B1055001 -n BLOSSOM "$work/basic-fat32.img"
printf 'BLOSSOM FAT32 fixture\n' > "$work/HELLO.TXT"
printf 'directory and long name\n' > "$work/long file name.txt"
mcopy -i "$work/basic-fat32.img" "$work/HELLO.TXT" ::/HELLO.TXT
mmd -i "$work/basic-fat32.img" ::/SUBDIR
mcopy -i "$work/basic-fat32.img" "$work/long file name.txt" '::/SUBDIR/long file name.txt'
fsck.fat -n "$work/basic-fat32.img"
mv -fT -- "$work/basic-fat32.img" "$out"
