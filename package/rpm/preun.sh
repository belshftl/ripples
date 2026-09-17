# $1 is 0 on erase and 1 on upgrade

if [ "$1" -eq 0 ] && [ -d /run/systemd/system ]; then
	systemctl stop 'ripples@*.service' || true
fi
