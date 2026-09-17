systemd-sysusers ripples.conf

if [ -d /run/udev ]; then
	udevadm control --reload || true
fi
# the modules-load.d entry covers future boots but for now load it manually
modprobe uinput 2>/dev/null || true
if [ -d /run/udev ]; then
	udevadm trigger --action=change --settle /sys/devices/virtual/misc/uinput 2>/dev/null || true
fi

if [ -d /run/systemd/system ]; then
	systemctl daemon-reload || true
fi
