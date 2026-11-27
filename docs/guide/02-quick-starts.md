# Quick starts

Each one starts from computers that are installed and in the site
([Getting started](01-getting-started.md)). Names in CAPITALS are your computers' names;
yours will differ.

- [Live captions](#live-captions)
- [Listening on phones](#listening-on-phones)
- [A presenter's laptop or a webcam](#a-presenters-laptop-or-a-webcam)
- [A second campus](#a-second-campus)

Running a service (switching cameras, mixing, streaming, recording) is done in **the control application**,
which drives IDN Media's switcher and desk; these quick starts set up what the computers
and the site do.

## Live captions

Captions turn the preacher's words into text on the stream (closed captions) and on
screen (subtitles). It all runs on the computer itself; nothing goes to the internet.
A computer with a graphics card gives the best captions.

1. Pick the computer that makes them (it may be BOOTH). In its IDN Node panel, find
   *Live captions*.
2. Tick **Caption on this machine**.
3. **Sound source:** the mixer, `MIXER-PC (Mixer)`. Its main mix is used. For a mix of
   the pulpit microphone alone, make it a matrix in the mixer and add `|ch=37` (matrix 1;
   39 is matrix 2).
4. **Names and words to spell right:** your pastors' names, books, place names.
5. **Save.** The first time, the speech model downloads (75 MB to 1.6 GB).
6. In the switcher (from the control application), turn its captions on from the captions source: they
   go out in the stream and the recordings, and a caption title shows the last two lines
   as they are spoken.

## Listening on phones

People listen on their own phones, in the browser, with no app: a translation, a
hearing-assistance mix, the overflow room. The page they open (and its QR code) is
the control application's; IDN Media sends the sound.

1. In the IDN Node panel of a computer on the same network as the church Wi-Fi, find
   *Listening on phones* → **Change**.
2. **Add a channel** for each: the name people see (`English`), the source
   (`MIXER-PC (Mixer)`), and the first of its two channels: empty for the main mix,
   `37` for matrix 1, `39` for matrix 2. Put the interpreter's mix on a matrix.
3. **Save.** The control application's listening page then plays these channels from this computer.

Each listener uses about 64 kbit/s of the Wi-Fi; 100 phones need about 6.4 Mbit/s.

## Sound to and from your audio editor

REAPER, Bitwig, Studio One, Cubase and other audio editors get two plugins, installed with
IDN (the "Audio editor plugins" part of the installer; on Linux copy `IDN.clap` to
`~/.clap` and `IDN.vst3` to `~/.vst3`).

- **IDN Send** on a track sends that track to the network, as `YOUR-PC (REAPER Send 1)`.
  The mixer, the switcher, another editor or any IDN or NDI program can take it. The
  track itself plays on as before.
- **IDN Return** on a track plays into it whatever you route to `REAPER Return 1` in the
  site's audio routing (the control application; the computer's IDN Node must be running).

Each plugin has one setting, **Number** (1 to 64): use a different number for each send
and each return in a project. The number is saved with the project. Nothing is sent or
returned while the editor renders a mixdown faster than real time.

## A presenter's laptop or a webcam

Install IDN on the laptop and join it to the site, then in its IDN Node panel:
*Screens and cameras* → **Change** → **Add a screen** (`1` is the main screen) or
**Add a webcam** → name it → **Save**. It appears on the network as
`LAPTOP (the name)`, ready for the switcher. It sends while someone is logged in.

A guest's laptop without IDN: use an HDMI capture card or an NDI converter, or let them
push to the switcher with OBS: add a stream input to the switcher
(`rtmp://0.0.0.0:1935/live/KEY`, from the control application) and give them `rtmp://BOOTH:1935/live/KEY`
for OBS's stream settings.

## A second campus

Two locations share sources over the internet, encrypted.

1. **At the main site:** pick a computer for the link. Its IDN Node panel → *Link to
   another location* → **Change** → **Is the main site's gateway**. Note the UDP port
   (29205) and forward it to this computer on the router.
2. **At the other campus:** no Site Manager is needed. Install IDN on one computer
   there, and in its IDN Node panel → *Link to another location* → **Links this site to
   the main site**, with the main site's public address (or name). It joins the main
   site through the link; approve it as usual (the control application). The campus's other
   computers then find the site through it and join the same way.
3. **Bandwidth each way:** what the internet connection can spare, such as 40 Mbit/s.
   Video needs a graphics card's encoder.
4. **Groups that cross:** only sources in these groups go across. `public` is every
   source unless you have made groups.

The Site Manager (in the control application) shows the far end, the round trip and the bandwidth in use.
