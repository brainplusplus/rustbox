# Verifying a macOS build without an Apple Developer ID

You asked whether it can be done without a paid Apple Developer ID. **Yes, for
running it yourself on your own Mac.** No, for handing a smooth download to
someone else.

## What each level actually allows

| Approach | Runs on your Mac | Runs after download by someone else |
| --- | --- | --- |
| Unsigned | Sometimes, often blocked when a framework is loaded | Blocked, needs a manual override every time |
| Ad-hoc signed (`codesign -s -`) | **Yes** | Blocked until the user overrides Gatekeeper once |
| Developer ID + notarized | Yes | Yes, no warning |

Ad-hoc signing is free and needs no Apple account. It is what `run-macos.sh`
does. It satisfies the local code-signing checks that macOS applies when a
process loads a framework and spawns helper processes, which is exactly what CEF
does.

## The manual override, exactly

For a build copied to another Mac (or a downloaded dmg), macOS attaches a
quarantine flag and refuses to launch. The user does one of these once:

```sh
# 1. Remove the quarantine flag (what the flag actually is: a download marker).
xattr -dr com.apple.quarantine /Applications/RustBox.app

# 2. Or let Gatekeeper re-check it and allow it explicitly.
sudo spctl --add --label RustBox /Applications/RustBox.app
```

Or, with no terminal at all: right-click the app, choose **Open**, then **Open**
again in the dialog. That writes the exception. Double-clicking will not.

Notes that matter:

- `spctl --master-disable` (turning Gatekeeper off globally) also works and is
  the wrong answer: it weakens the whole machine for one app.
- With CEF inside, this is not a one-time thing per download: every new build is
  a new unsigned binary, so the override has to be repeated per build.
- Telling users "right-click and Open" is a support cost that never goes away.
  That is the real reason a Developer ID exists, not a technical limit.

## What still works without a Developer ID

- Building and running on your own machine. That is enough for M0 through M6.
- Verifying the CEF work end to end locally: window, page load, message loop,
  proxy, storage, downloads.
- Producing a dmg you can install on your own machines.

## What does not work without one

- A download that opens without a warning for anyone else.
- Homebrew cask style distribution (`brew install --cask`), which relies on a
  verified signature.
- Avoiding the "damaged and can't be opened" message, which is what an
  ad-hoc-signed app inside a quarantined dmg often shows. The fix is the
  `xattr` command above, but users read that message as "the app is broken".

## For this project, in practice

M0 through M6 need no Developer ID at all. M7 (packaging) does, if the dmg is
ever meant for someone other than you. The plan records that as a blocker for
M7 only, not for the work before it.

## Verifying the spike on a Mac

```sh
cd tools/cef-spike
./run-macos.sh
```

The script builds, assembles the `.app`, signs it ad-hoc inside out (nested code
before the outer bundle), verifies the signature, clears any quarantine flag, and
runs. What to read in the output is described in `README.md`: the `pumps=` value
must keep climbing, and a page title should appear.
