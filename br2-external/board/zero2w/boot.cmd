# A/B slot selection for RAUC (compiled to boot.scr by mkimage).
# BOOT_ORDER lists slots by priority; each boot uses up one try of the first
# slot with tries left. Linux resets the count with "rauc status mark-good"
# once the system is healthy. A slot that fails 3 times in a row is skipped,
# which rolls back to the other slot.

# bootflow sets these when it runs this script; default to the SD card
test -n "${devtype}" || setenv devtype mmc
test -n "${devnum}" || setenv devnum 0

test -n "${BOOT_ORDER}" || setenv BOOT_ORDER "A B"
test -n "${BOOT_A_LEFT}" || setenv BOOT_A_LEFT 3
test -n "${BOOT_B_LEFT}" || setenv BOOT_B_LEFT 3

setenv bootpart
setenv raucslot
for BOOT_SLOT in "${BOOT_ORDER}"; do
	if test "x${bootpart}" != "x"; then
		# a slot was already chosen
	elif test "x${BOOT_SLOT}" = "xA"; then
		if test ${BOOT_A_LEFT} -gt 0; then
			setexpr BOOT_A_LEFT ${BOOT_A_LEFT} - 1
			echo "Booting slot A, ${BOOT_A_LEFT} tries left after this"
			setenv bootpart 2
			setenv raucslot A
		fi
	elif test "x${BOOT_SLOT}" = "xB"; then
		if test ${BOOT_B_LEFT} -gt 0; then
			setexpr BOOT_B_LEFT ${BOOT_B_LEFT} - 1
			echo "Booting slot B, ${BOOT_B_LEFT} tries left after this"
			setenv bootpart 3
			setenv raucslot B
		fi
	fi
done

if test -z "${bootpart}"; then
	echo "No slot has tries left; resetting both to 3"
	setenv BOOT_A_LEFT 3
	setenv BOOT_B_LEFT 3
	saveenv
	reset
fi

saveenv

setenv bootargs "root=/dev/mmcblk0p${bootpart} rootfstype=ext4 rootwait console=ttyAMA0,115200 console=tty1 panic=5 rauc.slot=${raucslot}"

# The firmware loaded the device tree (with config.txt overlays) at ${fdt_addr}
if load ${devtype} ${devnum}:${bootpart} ${kernel_addr_r} /boot/Image; then
	booti ${kernel_addr_r} - ${fdt_addr}
fi

# Kernel missing or unbootable: the try is already used up, so reset and
# let the countdown move on to the other slot.
echo "Slot ${raucslot} failed to boot; resetting"
reset
