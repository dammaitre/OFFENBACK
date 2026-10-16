# OFFENBACK - In terminal midi keyboard controller

Everything is to be done from scratch upon instructions on this file. Global guidelines :
 + Terminal minimalistic app controlled by computer keyboard
 + Input is MIDI from one of computer ports, should be chosable. Also for 
 tests, the fake_keyboard utility mimics an MIDI input for a few minuts
 + Output : computer speakers / bluetooth gate. 
 + Hijack the MIDI flow with our own sound profiles and parameters. I'd want to use for instance MuseHub's open musesounds as grand pianos. They are at ~/Muse Sounds
 + Impose custom sound curve over the recieved midi input, auto sustain, and other effects (reverb, cut offs, detune, pitch, and other that I'll ask for later)
 + Minimal latency between key press and bluetooth sound
