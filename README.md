# ripples

is a dead simple keyboard debouncer for Linux.

-----

## Install / setup

The [releases page](https://github.com/belshftl/ripples/releases/latest) contains prebuilt packages/binaries.

### Debian/Fedora

If you are on Debian 12+ or Fedora 43+, use the `.deb` / `.rpm` packages. Download one suitable for your arch; if you're unsure what the right one is or what that means, it's most likely `x86_64`/`amd64` (they're the same thing). Install via:
```sh
# on Debian:
sudo apt install ./ripples_0.1.0-1_amd64.deb

# on Fedora:
sudo dnf install ./ripples-0.1.0-1.x86_64.rpm
```

Remove like a normal package, via `sudo apt remove ripples` / `sudo dnf remove ripples` or their sibling "purge" options.

### Other distros

Download the executable suitable for your arch/libc; the musl ones are statically linked. Make it executable (`chmod +x`) and install it somewhere in `$PATH`, e.g. `/usr/local/bin/`. This is sufficient to manually invoke it from the command line.

Alternatively, build from source by cloning this repo and running `cargo build --release`. The binary is placed in `target/release/ripples`. This is not necessary if you don't plan to edit the code or build for a platform there isn't a pre-compiled executable for.

On systemd distros, to install the systemd service (and with it the right sysusers/udev rules), make sure to install it as specifically `/usr/bin/ripples`, as the service unit looks for that exact path. Then, clone this repo and run all of the following **as root**:
```sh
install -D -m644 package/ripples@.service /etc/systemd/system/
install -D -m644 package/ripples.sysusers /etc/sysusers.d/ripples.conf
install -D -m644 package/99-ripples.rules /etc/udev/rules.d/
install -D -m644 package/ripples.modules-load /etc/modules-load.d/ripples.conf
install -m644 package/ripples.conf /etc/
systemd-sysusers
udevadm control --reload
modprobe uinput || true
udevadm trigger --action=change /sys/devices/virtual/misc/uinput
systemctl daemon-reload
```

To undo the above, uninstalling the systemd service and sysusers/udev rules (also all as root):
```sh
systemctl stop 'ripples@*.service'
rm -f /etc/systemd/system/*.wants/ripples@*.service /etc/systemd/system/ripples@.service
rm -f /etc/udev/rules.d/99-ripples.rules /etc/sysusers.d/ripples.conf /etc/modules-load.d/ripples.conf
rm -f /etc/ripples.conf # if you want to get rid of the service config too
groupdel ripples-uinput
udevadm control --reload
systemctl daemon-reload
```

-----

## Usage

Run `sudo ripples -l` to see a list of every available device, something like:
```
/dev/input/event3 | 0627:0001 | USB  | QEMU QEMU USB Keyboard | keyboard
/dev/input/event2 | 0627:0001 | USB  | QEMU QEMU USB Mouse    | probably not a keyboard
/dev/input/event1 | 0627:0001 | USB  | QEMU QEMU USB Tablet   | probably not a keyboard
/dev/input/event0 | 0000:0001 | Host | Power Button           | probably not a keyboard
```

Find your keyboard there; in this example, it's `/dev/input/event3`. Run `ripples -p <your keyboard device>` to get a longer path to it that persists across reboots; for example, `ripples -p /dev/input/event3` here prints:
```
/dev/input/by-id/usb-QEMU_QEMU_USB_Keyboard_68284-0000:00:04.0-3-event-kbd
```

Copy whatever you got, and enable the systemd service instance:
```sh
sudo systemctl enable --now "$(systemd-escape --template ripples@.service --path <long path>)"
```
Replace `<long path>` with the longer path you just copied. There is one instance per keyboard.

That's it. The default settings are eager-press defer-release debouncing with a 20ms window. To adjust them, place your command-line options into `/etc/ripples.conf`, which is where the systemd service reads them from; see the "Configuration" section below for the available options.

If you don't use systemd, ripples can be manually started with `sudo ripples /dev/input/...`. When writing a custom OpenRC/runit/s6/etc. service: use the `--wait-for-device` option, and see [the systemd service source](package/ripples@.service) as a quick reference, though you may have to either hardcode in the device path or devise an alternative means of configuration for the device. Stable `/dev/input/by-id/` or `/dev/input/by-path/` symlinks also need udev/eudev, so if you don't have that you'll need a custom search script. Good luck out there.

-----

## Configuration

The debounce mode can be set with the `-m`/`--mode` option. Available debounce modes, given a debounce window `w`, are:
- `eager`: if a press happens, it gets reported, then the key state is entirely ignored for `w`, then the new key state after the window is resampled and reported. Technically lowest bidirectional latency, in practice very situational, and a bounce longer than `w` doesn't get properly filtered out.
- `defer`: a press/release is only reported if the physical key has been pressed/released for `w`. Adds `w` of latency to keypresses; also situational, but may sometimes be useful if the switch is bad enough that only a stability requirement will do.
- `mixed`: a press is reported immediately, whereas a release is only reported if the key has been released for `w`. In other words, eager-press defer-release. This is the default, and it's sufficient almost all of the time: keypress latency isn't affected, and chatter gets filtered.

The window is adjustable with the `-w`/`--window` option, which takes a value in milliseconds. The default is 20ms; this is already enough to catch most chatter, but for reference, I wasn't able to get a deliberate double-tap of one key with two fingers faster than 60ms. Raise it until chatter stops, lower it if regular double-taps start getting caught.

Which keys get debounced can be filtered with `--keys` or `--exclude-keys`; the former makes only the specified keys debounced, and the latter makes *all but* the specified keys debounced. They take comma-separated lists of keys, either by evdev key name (`KEY_A`, or just `A`) or by evdev keycode (`30` or `0x1e`), case-insensitive. Examples:
```
--keys CAPSLOCK
--exclude-keys W,A,S,D
```
Keys that aren't debounced get passed through as-is. You can find the list of evdev key names straight from the Linux kernel sources [here](https://github.com/torvalds/linux/blob/45065a5095c7773fb98c35d60c20c3b513540597/include/uapi/linux/input-event-codes.h#L76).

That's pretty much it. See `-h`/`--help` for all of the options; there are some miscellaneous ones regarding hotplug and virtual device naming.

-----

## Current limitations/notes

- Only key/scancode/syn events from the keyboard are carried through; a composite device that also reports, say, relative/absolute axes would have those events be dropped. ripples warns at startup if this is the case.
- LED events and repeat settings sent to the virtual device get forwarded to the real keyboard, but not beeps (`EV_SND`) or rumble (`EV_FF`).
- Unplugging the keyboard prints `Failed to ungrab device: No such device`. This happens because of a `Drop` impl from the `evdev` crate, which ripples uses, and is harmless; ripples waits for the replug like usual.
- There is no per-key configuration yet; the same window/mode applies to every debounced key.
