# Dropfin

A small, native Mac app for moving movies and shows onto your media server, with a Finder-style window.
It connects over **SSH/SFTP** (the same login you'd use with `scp`), so **nothing gets installed on the server**.

- Drag movies (or whole folders) from Finder into any folder on the server
- Fast, resumable transfers. If the Wi-Fi drops or you pause, it continues where it left off.
- Rename, new folder, move (drag, or ⌘X then ⌘V), copy, delete, Get Info, and Quick Look for subtitles, NFO files, images and MP4s
- Download files and folders to your Mac's Downloads folder
- Optional Jellyfin hookup: your libraries appear in the sidebar, and a library scan runs automatically when uploads finish
- About 10 MB. Written in Rust, using the Mac's built-in WebKit for the interface.

---

## Getting the app: GitHub builds it for you

A Mac app has to be compiled on a Mac. GitHub provides free Mac build machines, so you don't need Xcode or anything else installed.
You only do this once. After that, every change rebuilds the app automatically.

### 1. Put this project on GitHub

1. Sign in at **github.com**. A free account works.
2. Click **+** (top right) → **New repository**.
3. Name it `dropfin`. **Private** is fine. Don't tick "Add a README". Click **Create repository**.
4. Upload the files using one of these options:
   - **Easiest:** tell Claude the repository name (for example `your-name/dropfin`) and it pushes the code for you.
   - **GitHub Desktop:** File → Add Local Repository → choose this folder → Publish.
   - **Terminal**, from inside this folder:
     ```bash
     git remote add origin https://github.com/YOUR-NAME/dropfin.git
     ```
     ```bash
     git push -u origin main
     ```

### 2. Let GitHub build it (about 10 minutes)

Open the repository's **Actions** tab. You'll see **Build Mac app** running. When it shows a green tick, it's done.
If GitHub asks you to enable Actions for the repository, click the button to allow them.

### 3. Download and install

1. Go to the repository's main page → **Releases** (right-hand side) → **Dropfin — latest build**.
2. Download **Dropfin.dmg**, open it, and drag **Dropfin** onto **Applications**.
3. The first time you open it, macOS warns that it can't verify the developer. Dropfin isn't from the App Store or notarized by Apple.
   Go to **System Settings → Privacy & Security**, scroll down, and click **Open Anyway**.
   Or run this once in Terminal:
   ```bash
   xattr -cr /Applications/Dropfin.app
   ```
4. If macOS asks to let Dropfin find devices on your **local network**, or to access your **Downloads** folder, click **Allow**.

---

## Using it

The first time, Dropfin looks for Jellyfin on your network and fills in the address. Enter the username and password you use for SSH on that machine, then press **Connect**.
The password is stored in your macOS Keychain. After that it's one **Connect** button, and you land back in the last folder you used.
If you already use SSH keys (`~/.ssh/id_ed25519` or an ssh-agent), leave the password empty.

### What the server needs

Only SSH, which most media servers already have:

| Server | Turn on SSH |
| --- | --- |
| Ubuntu / Debian | `sudo apt install openssh-server` |
| Synology | Control Panel → Terminal & SNMP → Enable SSH, and File Services → FTP → Enable SFTP |
| Unraid | Settings → Management Access → SSH |
| Windows | Settings → Optional features → OpenSSH Server |
| Mac | System Settings → General → Sharing → Remote Login |

The SSH user must be allowed to write to your media folders. If uploads fail with "Permission denied", give your user the folder on the server, for example:

```bash
sudo chown -R youruser /srv/media
```

### Optional: Jellyfin

Open **Dropfin → Server Settings** (⌘,) and add your Jellyfin address and an API key. In Jellyfin, create the key under **Dashboard → API Keys → +**.
Your libraries then appear in the sidebar, and Dropfin starts a library scan when uploads finish.
If Jellyfin runs in Docker, the paths it reports may be paths inside the container. Those libraries show greyed out.
Browse to the real folder instead and choose **Add to Sidebar**.

### Updates

Every time GitHub builds a new version, copies of Dropfin built by GitHub notice it on their own.
A tiny dot appears next to **…** at the bottom of the sidebar, and that menu gains **Update Available…**.
You can also check yourself from **Dropfin → Check for Updates…**.
Updating means downloading the new `Dropfin.dmg` and replacing the app in Applications. Your servers and settings stay.

The check is one small request to GitHub, at most a few times a day. It only works if the repository is **public**,
because GitHub doesn't show private repositories' releases without signing in. With a private repository, the check quietly does nothing.

### Keyboard

| | |
| --- | --- |
| ⌘↓ / double-click | Open |
| ⌘↑ / ⌘[ / ⌘] | Enclosing folder / Back / Forward |
| ⇧⌘G | Go to Folder (type a path) |
| ⇧⌘N | New Folder |
| ⌘U / ⇧⌘U | Upload files / Upload a folder |
| ↩ | Rename |
| Space | Quick Look |
| ⌘C ⌘X ⌘V | Copy, Cut, Paste (Cut + Paste moves) |
| ⌘⌫ | Delete |
| ⌘D | Download to Mac |
| ⌘1 / ⌘2 | Icons / List |
| ⌘I | Get Info |
| Hold ⌥ while dragging | Copy instead of move |

### Good to know

- Uploads go to a hidden `.name.dropfin-part` file, which is renamed only when complete, so Jellyfin never sees a half-copied movie.
  If a transfer is interrupted, dropping the same file again resumes it.
- `.DS_Store` and other hidden macOS files are skipped when you upload folders.
- On first connection Dropfin remembers the server's SSH identity, and warns you if it ever changes.
- The interface is served only to the app itself, on `localhost`. Other devices on your network can't reach it.
- Settings live in `~/Library/Application Support/Dropfin`. To uninstall, delete the app and that folder.
  Saved passwords are in Keychain Access under "Dropfin".

---

## For developers

| Path | What it is |
| --- | --- |
| `src/api.rs` | Local HTTP API used by the interface |
| `src/ssh.rs` | SSH/SFTP: auth, host-key pinning, file operations |
| `src/jellyfin.rs` | Jellyfin LAN discovery, libraries, scans |
| `src/gui.rs` | Native window (tao + wry + muda) |
| `web/index.html` | The whole interface (HTML/CSS/JS, embedded in the binary) |
| `scripts/bundle-macos.sh` | Builds the universal `.app`, `.dmg` and `.zip` on a Mac |

Run the app:

```bash
cargo run
```

Run only the helper, then open `http://localhost:8097` in a browser (no window, useful for testing):

```bash
cargo run --no-default-features -- --server-only --port 8097
```

## Credits

Icons: [Font Awesome Free](https://fontawesome.com) 7.3.1 (icons CC BY 4.0, font SIL OFL 1.1), bundled in `web/fa/`.
