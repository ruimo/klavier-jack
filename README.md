# Jack interface for klavier-core

## Prepare

### Linux(Debian)

You need to install jack.

    $ sudo apt install qjackctl pulseaudio-module-jack libjack-jackd2-dev

Launch qjackctl.

    $ qjackctl
    
* Driver: alsa.
* Interface: (default)
* Sample Rate: 22kHz is sufficient for MIDI sequencer.
* Frames/Period: 1024 or so.
* Periods/Buffer: 5 (for example)<br/>
If you specify smaller value, your application should run faster enough to send MIDI data. If you specify bigger value, some amount of time will be needed to start playing MIDI, i.e. the latency. (If you specify 5, 5 * 1024(=Frames/Period) / 22050(Sample Rate) = 0.23 secs would become the latency.)
* MIDI Driver: seq.

Click Start on qjackctl before starting your application. Once your application started, clickg Graph on qjackctl. Your application will be shown with the name you specified in the first argument of jack::Client::new(). Your MIDI interface will be shown with the name 'system'. Connect your application's output to the system's MIDI interface.

## Example

After the 'Prepare' described above, you can run sample.

    $ cargo run --example sample01

# Misc

Ticks per quarter(TPQ): 240
Tempo: The number of quarter notes per minute.
Tick length (ms): 1000 * 60 / tempo / TPQ

Sampling Rate(SR): 22050
Cycle(ms): 1000 / SR

1 tick per cycle: SR * 60 / tempo / TPQ

1 cycle per tick: tempo * TPQ / SR / 60
