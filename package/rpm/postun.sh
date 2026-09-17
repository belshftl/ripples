# $1 is 0 on erase and 1 on upgrade

if [ "$1" -eq 0 ]; then
	# instances created with `systemctl enable` aren't removed by anything else
	rm -f /etc/systemd/system/*.wants/ripples@*.service
	if [ -d /run/udev ]; then
		udevadm control --reload || true
	fi
fi

if [ -d /run/systemd/system ]; then
	systemctl daemon-reload || true
	if [ "$1" -ge 1 ]; then
		systemctl try-restart 'ripples@*.service' || true
	fi
fi
