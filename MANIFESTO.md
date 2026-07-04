Mizu means water: transparent, clean, still.

No legacy to carry, which leaves room to do things differently. Mizu isn't a general programming language, and it isn't trying to match everything HTML5 and JavaScript can do. It's a way to describe a document and let it react: what a good hypermedia format is supposed to do.

The point is one thing: a page you receive from a stranger should be safe to open. Today a web page is an open-ended program. It can run as long as it likes, pull in more code as it goes, and reach wherever it wants, and you have to trust all of it before you see any of it. Mizu chooses to do less, on purpose, because doing less is the guarantee. A Mizu document can be read, checked, and drawn. Every reaction it makes is finite, runs only code that shipped with it, and reaches only the addresses it named first. You can see everything it could possibly do before you let it do anything.

Four things follow from that, so far.

**Every reaction ends.** No loops, no recursion; the call graph is checked before anything runs. A page can keep reacting for as long as you keep it open — a timer can tick again, a click can land again — but no single reaction can run forever. You never have to wonder whether a page will freeze. It can't.

**It reads top to bottom.** Nothing is used before it's declared, so the order you read is the order it runs. The document means exactly what it says.

**Nothing wakes itself up.** A timer or an event can be declared in advance, but what pulls the trigger comes from outside: the clock, the click, the response. The document reacts; it never acts on its own.

**Every name is known before it's reached.** Addresses, images, endpoints are declared in one place and checked before anything touches them. A link takes you somewhere; it doesn't run code.

This is for people who think a document should be something you can reason about, not something you have to defend against.

Still water finding its shape. No spec yet, just the idea, and the room to get it right.

mizvno
20/06/2026
