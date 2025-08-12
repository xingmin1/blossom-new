# sudo mount sdcard-la.img /mnt/sdcard/
# sudo cp ./apps/kilo/build/kilo-la64 /mnt/sdcard/
# sudo umount /mnt/sdcard 
# echo "Copied kilo-la64 to sdcard-la.img"

# sudo mount sdcard-rv.img /mnt/sdcard/
# sudo cp ./apps/kilo/build/kilo-riscv64 /mnt/sdcard/
# sudo umount /mnt/sdcard
# echo "Copied kilo-riscv64 to sdcard-rv.img"

sudo mount sdcard-la.img /mnt/sdcard/
sudo cp ./apps/tetris/build/tetris-la64 /mnt/sdcard/
sudo umount /mnt/sdcard 
echo "Copied trtris-la64 to sdcard-la.img"

sudo mount sdcard-rv.img /mnt/sdcard/
sudo cp ./apps/tetris/build/tetris-riscv64 /mnt/sdcard/
sudo umount /mnt/sdcard
echo "Copied tetris-riscv64 to sdcard-rv.img"