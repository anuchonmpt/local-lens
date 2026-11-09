# Changelog

## 0.1.7 · 2026-11-09

- Bundled kubectl v1.36.6 in the macOS installer for Apple Silicon and Intel.

## 0.1.6 · 2026-11-09

- Fixed cluster connections when launching the macOS app from Applications by making Homebrew and Google Cloud credential helpers discoverable by kubectl.

## 0.1.5 · 2026-11-09

- Added a custom macOS app icon for Finder and the Dock.

## 0.1.4 · 2026-11-09

- Moved Check and Update into the changelog, which opens from the version number in the footer.
- Check for updates when the app opens. The footer version turns green and shows an up arrow when an update is available.

## 0.1.3 · 2026-11-09

- Check for updates from the toolbar. The Update button is enabled only when a newer version is available.
- Click Update to install and reopen the app without an extra dialog.
- Moved the version number to the footer. Click it to open the changelog.

## 0.1.2 · 2026-11-09

- Download updates through the GitHub Release Assets API for more reliable access to the macOS installer.

## 0.1.1 · 2026-11-09

- Added a universal macOS disk image for Apple Silicon and Intel Macs.
- Added in-app update checks, release notes, and installation from GitHub Releases.
- Added the running version and version history to the Updates panel.

## 0.1.0

- Added local kubeconfig discovery, cluster switching, and namespace filtering.
- Added cluster and workload overviews, resource tables, details, and metrics.
- Added pod logs, port forwarding, and a 3D service topology view.
