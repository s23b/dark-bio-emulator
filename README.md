# Dark Bio - Ark Emulator

An emulated Ark enclave for development and demos. It boots the real ArkOS
firmware inside QEMU, with an optional native window showing the device's
physical face (4 RGB LEDs and a reset pin).

> [!CAUTION]
> **The emulator is not a vault.** The backing disk image is an unencrypted
> qcow2 file on your host filesystem. Anything the emulated Ark stores ends up there,
> readable by anyone with access to your machine. Do not put real genomic
> data, real keys, or anything else you want to keep private into the
> emulator. It exists for development, demos, and integration testing only.
> The cryptographic and physical-security properties Dark Bio promises apply
> to the actual Ark hardware, not this emulator. For any workload where data
> confidentiality matters, use a real Ark.

## Install

Download a build for your operating system and processor from
[GitHub Releases](https://github.com/dark-bio/emulator/releases). Releases
include QEMU and ArkOS, so you can launch the app without configuring either.

| Platform | Installer | Portable version |
|---|---|---|
| macOS | `.dmg` | Unzip the `.zip` and open the app |
| Windows | `.exe` | Unzip the `.zip` and run `ark-emulator.exe` |
| Linux | `.deb` | Make the `.AppImage` executable and run it |

On macOS, you can also install with Homebrew:

```sh
brew install --cask dark-bio/tap/ark-emulator
```

macOS requires version 15 (Sequoia) or later. Windows builds are unsigned;
if SmartScreen blocks a downloaded release, choose **More info**, then
**Run anyway**.

## Get started

1. Open **Ark Emulator**.
2. In the settings screen, use **New** beside the filename to create an
   emulator file, or **Open** to choose one you already have.
3. Choose **Save and start** to remember your choices and boot the emulator.
   Use **Start** to boot without saving your settings.
4. Open [Ark Hub](https://hub.dark.bio) in Chrome or Edge and connect to the
   running emulator.

Each `.ark` file stores one emulator's data. Open the same file to continue
where you left off, or create a new file to start with a blank device.

Click the notch below the device to see its information, then the gear to
open settings. **Autostart** starts your saved emulator automatically on the
next launch. The **developers** section contains memory and environment
settings. Settings saved while an emulator is running apply on a later launch;
an environment change applies only to a newly created device.

To close the window, use your operating system's close or quit shortcut,
such as Alt+F4 on Windows or Cmd+Q on macOS.

## Run multiple emulators

Launch the app again to open another window. On macOS, use **New Window**
(Cmd+N) in the app menu. Each running instance needs a different emulator file;
use **Open** or **New** in that window to choose one.

## Command line

Use `ark-emulator` to manage emulators without opening a window. Installing
with Homebrew, the Windows installer or the `.deb` makes the command
available in your terminal. On Windows, open a new PowerShell or Command
Prompt window after installing.

```sh
ark-emulator start --headless  # boot without a window, return when ready
ark-emulator list              # show what is running on this computer
ark-emulator button press      # hold its reset button
ark-emulator button release    # release the CLI hold
ark-emulator stop              # shut it down
```

Omit `--headless` to show the device window. For a foreground process, use
`ark-emulator --headless --image demo.ark`; Ctrl-C stops it. See
`ark-emulator help start` for readiness, timeouts and image defaults.
Button commands accept a locator, name, serial or image when several emulators
are running. `ark-emulator button press --release-after 3` schedules release
after 3 s, even after the command exits. Use `--release-after 0` to release
immediately after pressing. See `ark-emulator button press --help` for delivery
and hold behavior.

For the portable Windows ZIP, run `.\bin\ark-emulator.cmd` instead of
`ark-emulator` from the extracted folder. On macOS and Linux, run the
executable inside the portable build directly.

Hand the locator to [`ark`](https://github.com/dark-bio/cli), which talks to an
emulated Ark exactly as it talks to hardware:

```sh
ark -d emulator:18181 status
```

A fresh emulator has a self-signed identity. `ark enroll` prints the Ark Hub
address that gives it an attested one, valid for 30 days; after that,
`ark-emulator stop`, `wipe` and `start` give a fresh device to enroll again.

`ark-emulator --help` lists every command and option, and `ark-emulator help`
names the reference topics, whose sources in
[`launcher/src/cli/help`](launcher/src/cli/help) read the same on GitHub. AI agents
should read [`ark-emulator help agents`](launcher/src/cli/help/agents.md) first,
then `ark help agents` before driving the Ark itself. Apps to run on an
emulator, in Rust, Go, C and Python, are at
[examples](https://github.com/dark-bio/examples). Pairing and unlocking happen
in Ark Companion, on
[iOS](https://apps.apple.com/app/id6751324700) or
[Android](https://play.google.com/store/apps/details?id=bio.dark.companion).

## Build from source

Install Rust, the [Tauri system prerequisites](https://v2.tauri.app/start/prerequisites/),
and QEMU, including `qemu-img`. Download a matching kernel and initramfs pair
for your processor (`arm64` or `amd64`) from
[the emulator firmware releases](https://github.com/dark-bio/emulator-images/releases).
Source builds do not bundle QEMU or firmware.

From this repository, run:

```sh
cargo build --release -p launcher
./target/release/ark-emulator \
  --kernel /path/to/kernel \
  --initrd /path/to/initrd.gz
```

For everything the executable can do, run
`./target/release/ark-emulator --help`.

Linux amd64 packaging builds a reduced QEMU from the pinned submodule in an
Ubuntu 22.04 container. Docker and Make are required in addition to Rust:

```sh
git submodule update --init --depth 1 third_party/qemu
make qemu-linux
make qemu-linux-check
```

The build stages QEMU in `launcher/binaries` and `launcher/qemu-libs` for
Tauri packaging. Its runtime and corresponding-source archives are in
`target/qemu-linux/artifacts`. The source archive includes the dependency
sources and build recipes; GitHub's automatic repository archive omits
submodule contents. Each Linux release publishes the source archive beside
the installers. `QEMU_JOBS` controls compiler parallelism and `QEMU_OUTPUT`
selects the build directory. The builder's module documentation describes
the packaging and checks.

## Layout

| path | role |
|---|---|
| `launcher/` | Rust runtime and optional Tauri window. Owns QEMU, the hardware connection and discovery, and carries the packaging config and macOS entitlements. |
| `ui/` | Static HTML/CSS/JS. Renders device state from Rust and forwards user interactions through Tauri commands. |
| `docs/` | Maintainer documentation. Currently the one-time Apple Developer setup the macOS signing in CI depends on. |
| `third_party/qemu/` | Upstream QEMU source pinned as a Git submodule. |
| `tools/qemu-build/` | Linux QEMU builder, dependency and source packaging, and isolated runtime checks. |
| `.github/` | CI and release support. `scripts/` gathers firmware and the macOS and Windows QEMU runtimes. `packaging/` holds the Linux QEMU configuration and the Homebrew cask template. |
