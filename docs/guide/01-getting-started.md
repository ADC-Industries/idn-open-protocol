# Getting started

IDN Media is the engine: it carries pictures and sound between your computers and runs
the switcher, the sound desk, the network audio device and the site. The screens people
work with on a Sunday (the switcher, the desk, the site's administration) are those of
**the control application** that drives IDN Media (the program the church runs its
services from). This guide covers installing IDN Media, each computer's IDN Node panel,
the network and the sound, and what to do when something doesn't work.

## The pieces

**A site** is all the IDN computers in your church or building. They share one network,
one set of encryption keys and one place to manage them.

**The IDN Node** runs on every computer of the site. It is the computer's membership of
the site: it holds the keys, takes the computer's settings, and runs the jobs a computer
does for the site (the switcher, the desk, sending a screen or a webcam, listening on
phones, captions). Its **control panel** opens in the browser: **IDN Node** in the Start
menu (or the applications menu), or `idn-node open`. In each desktop session a small
program with no window, **idn-session**, does the Node's part there (sound from programs,
screens, webcams); it starts at every log-on.

**The IDN Site Manager** runs on one computer that is always on. It keeps the site
together: which computers belong, how sound is routed between them, updates, backups,
alerts and logs. You work with it from the control application (it has an API and no page of its own).

**The IDN Switcher** is the video desk's engine. It takes cameras, slides, videos and
titles and makes the program: the livestream, recordings and screens. It runs on a
computer as one of its Node's services; the control application is its screen.

**The IDN Mixer** is the sound desk's engine: microphones, the band, the presentation's
sound, feeding the speakers, the stage monitors, the livestream and phones in the room.
The control application is its screen too.

**IDN Data** keeps a media library and downloads the videos you play. It also reads
spreadsheets and feeds for the titles (a sermon series, scores).

**OBS Studio** and **vMix** work alongside. OBS gets IDN sources through the IDN plugin
the installer adds, and vMix through Open Media Transport (OMT).

## What you need

- **A wired network.** Video over Wi-Fi drops pictures and lags. Phones listening in the
  room use Wi-Fi; that's fine.
- **Windows 10 or 11 (64-bit), or Linux**: Debian 12, Ubuntu 24.04 or Ubuntu 26.04
  (64-bit), with a package (see [Linux](#installing-on-linux)).
- **A graphics card (GPU) on the switcher's computer.** It is needed for 4K at 60
  pictures a second. 1080p and 4K at 30 run on the processor alone if they must.
  A typical church runs 4K at 30 or 1080p.
- **An internet connection** for the livestream: 6 Mbit/s or more upload for 1080p.

## Installing on Windows

On every computer of the site:

1. Close OBS Studio if it's open.
2. Run **idn-media-setup-x64.exe** (IDN Media's installer). Windows asks for administrator rights; say yes.
3. **Components.** Keep them all ticked: the IDN Node, the tools, the OBS plugin
   (installed only if OBS is there) and FFmpeg (downloaded, 80 MB; the switcher,
   recordings and video files need it).
4. **How the IDN Node runs.** Choose **As a service (recommended)**. The computer is
   then in the site from start-up, before anyone logs on. Choose **When you log on**
   only on a shared computer where each person should be separate.
5. **Join a site.** If the site already has a passphrase, type it here, and the computer
   joins the site with no approval needed. Otherwise leave both boxes empty: the IDN
   Node finds the Site Manager on the network and asks to join, and you approve it there
   (see [Adding a computer](#adding-a-computer-to-the-site)).
6. At the end, leave **Open the IDN Node control panel** ticked. The panel opens with a
   short setup (below).

The installer lets the IDN programs through Windows Defender Firewall **on private
networks only**. In Windows Settings → Network, make sure the church's network is set to
**Private**, not Public. A computer on a "Public" network can't see or be seen by the
others.

The Start menu then has a folder **IDN Media** with **IDN Node** (the control panel),
**IDN command prompt**, **IDN Guide** and **Uninstall IDN Media**. The switcher and the
sound desk have no shortcuts: they run in the background, and the control application is where you use
them. At every log-on Windows starts **idn-session**, the IDN Node's side in your session
(no window): it runs this computer's sound, screens and webcams as the panel sets them
and shows a message when another computer asks to link or this computer waits for
approval.

Running the installer again upgrades the computer. It stays in the site.

## The first-run setup

The control panel greets a new computer with five short steps. Each answer is saved as
you give it, so you can stop part-way.

1. **Welcome.** A name for this computer, such as BOOTH, LOBBY-PC or STAGE. Everyone
   sees it in source names ("BOOTH (Program)"), so keep it short and clear.
2. **Join your site.** Pick the Site Manager found on the network, type its address, or
   choose **Not now**.
3. **Approve this machine.** A six-digit code appears, such as `482 913`. On the Site
   Manager (in the control application), approve the computer with that name and the same code. The page moves on by itself once it is approved.
   If you set the passphrase in the installer, there is nothing to approve. If you have
   a **join code** from the Site Manager, type it on the *Membership* page instead
   (below).
4. **What will this machine do?** Tick what fits: switch cameras and stream, mix the
   sound, let phones listen, send its screen or a webcam, sound from programs or a
   microphone. The settings for what you tick open when you finish.

**Set up again**, in the panel's *This machine* card, runs through it any time.

## Setting up the Site Manager

Choose a computer that is always on and on the church network: the booth computer, or a
small server. The Site Manager holds the site's keys, so keep that computer locked away
from the public, and back the site up (from the control application) once it is set up.

**On Windows, the installer sets it up:**

1. Run Setup on that computer and tick **IDN Site Manager (this computer runs the
   site)** on the components page. The IDN Node has to run as a service there (the
   default).
2. Choose the **site password** when Setup asks. The control application (or any program using the Site
   Manager's API, `https://THIS-COMPUTER:29202/api/v1/`) logs in with it.
3. This computer's own IDN Node joins the site like any other: approve it (below).

The IDN Node runs the Site Manager from start-up, before anyone logs on, and starts it
again if it stops; its *Services* card shows it. Its keys are kept in
`C:\ProgramData\IDN Site Manager`. Uninstalling IDN asks separately before removing
them — say **No** unless the site has moved to another computer.

**On Linux**, with the IDN Node installed as a service:

```
idn node set sitemanager-on=true
idn node set sitemanager-password=-
```

The second asks for the site password (at least 6 characters).

**By hand**, from a command prompt (it runs while the window is open):

```
idn discovery-server --trust-dir "C:\IDN Site"
```

then type `panel-password YOUR-PASSWORD` in the same window (the site password). Add `--passphrase
YOUR-PASSPHRASE` if you want computers to join with the passphrase and no approval.

The computers keep working while the Site Manager is stopped, but nothing new can join
until it runs again.

## Adding a computer to the site

**With a join code** (the quickest; a new site takes them):

1. Install IDN on the computer (above). It finds the Site Manager by itself.
2. In the control application, ask the Site Manager for a **join code**: six digits, such as `482 913`. It
   works once, for 10 minutes.
3. On the new computer, type it on the IDN Node panel's *Membership* page (*Join code*),
   or run `idn-node join 482913`. The computer joins at once. If the site is set to *code
   and approval*, approve it in the control application afterwards.

A wrong code shows "Wrong code (4 tries left)". After 5 wrong tries the code is used up:
ask for a new one. Type the code only on the computer you are adding. Never read it out
to anyone else.

**With approval:**

1. Install IDN on the computer (above). It finds the Site Manager by itself.
2. In the control application, open the site for new computers (or, on the Site Manager's console,
   `pairing`).
3. The new computer appears with its name, address and a six-digit code (`482 913`).
   Check that the code matches the one on the computer's IDN Node panel, then **approve** it (in the control application, or
   `approve ID` on the console).

How a site takes computers is its **join mode**, set in the control application:
- *Code or approval*: either way above. New sites start here.
- *Code and approval*: a join code, then approval.
- *Approval*: approval only. A site made before 2026-11-18 stays here until it is
  changed.

A computer with the site passphrase joins without being approved (except at a site set
to *code and approval*). A new site run by the
IDN Node opens itself for computers for its first half hour.

## Installing on Linux

Install the package (it brings what it needs from the distribution):

```
sudo apt install ./idn-media_0.3.8.beta22_amd64.deb
```

(the name of the file you downloaded; release downloads have a dot where the package's
version has ~). Keep the `./`: it tells apt the name is a file, not a package to look up.
It installs the IDN Node as a service,
started at once and at every boot, and adds you to the `idn` group: log out and in again.
The applications menu then has **IDN Node** and **IDN Guide**, and the session agent
(idn-session) starts at every log-in. The `idn` command and the other programs are on the
path. FFmpeg 8.1 comes with the package.

`idn-node open` (or IDN Node in the menu) opens the control panel, with the first-run
setup.

Updating: install the newer file the same way, with its own name; the machine keeps its
site identity.
Removing: `sudo apt remove idn-media` stops and removes the service and keeps the
machine's identity; `sudo apt purge idn-media` removes that too (the media folder,
`/var/lib/idn-media`, stays).

The Industrial edition's Node is its own package: `sudo apt install
./idn-industrial_0.3.8.beta22_amd64.deb` (again the name of the file you downloaded),
then `sudo idn-industrial enrol IDNI1-…` with the token from the Site Manager |
Industrial.

## Opening the IDN Node panel

**IDN Node** in the Start menu (or the applications menu) opens this computer's control
panel in the browser, with a one-time link: other programs and web pages can't reach it.
From a command prompt, `idn-node open` does the same. From another computer, give the
Node a network password: see
[Using the panel from another computer](05-machines-and-site.md#using-the-panel-from-another-computer).

The switcher and the sound desk run as the computer's services: tick them in the panel's
**Services** card, and they start with the computer (or when someone logs on) and again if
they stop. The control application connects to them; they have no window of their own.
