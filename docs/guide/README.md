# The IDN guide

For the people who set up and look after a church's IDN Media: the computers, the
network, the sound and the site. IDN Media is the engine behind your **control
application** (the program the church runs its services from): the screens used on a
Sunday (the switcher, the sound desk, the site's administration) are the control
application's. This guide shows where to click in each computer's IDN Node panel; you won't need to
program anything.

**Setting up for the first time?** Read [Getting started](01-getting-started.md), then
follow the [quick starts](02-quick-starts.md).

**Something not working?** Go to [Troubleshooting](07-troubleshooting.md).

## The chapters

1. [Getting started](01-getting-started.md): the pieces, installing them, joining
   machines to your site, opening the IDN Node panel.
2. [Quick starts](02-quick-starts.md): captions, listening on phones, an audio editor,
   a presenter's laptop or a webcam, a second campus.
3. [The switcher](03-switcher.md): what it does, running it, its API and MQTT.
4. [The mixer](04-mixer.md): getting sound in and out (cards, an X32, a network of
   Dante-compatible equipment), its API.
5. [Machines and the site](05-machines-and-site.md): each computer's IDN Node and what
   the site's IDN Site Manager does.
6. [For volunteers](06-volunteers.md): where the volunteers' screens are.
7. [Troubleshooting](07-troubleshooting.md): symptoms, causes and fixes.
8. [Reference](08-reference.md): network ports, where files are kept, and a glossary.

## The pieces in one picture

```
  cameras ──┐                                         ┌── YouTube, Facebook (stream)
  slides  ──┤                                         ├── recordings
  laptops ──┼──►  IDN Switcher  (pictures)  ──────────┼── projectors, confidence monitors
            │          ▲                              └── other rooms (overflow)
            │          │ the stream's sound
  mics    ──┼──►  IDN Mixer     (sound)     ──────────┬── speakers, stage monitors
  band    ──┘                                         ├── phones in the room (listening)
                                                      └── live captions

  Every computer runs an IDN Node. One computer also runs the IDN Site Manager,
  which keeps the site together: which machines belong, how the sound is routed,
  updates, backups and alerts. The control application is the screen for all of it.
```

Everything travels over the church's ordinary computer network. Use cables, not Wi-Fi,
for cameras and the computers that handle video.
