# mineshare

mineshare is a simple, no portforwarding proxy app for small Minecraft servers (1.8.x-1.21.x)

Essentially, it lets you _share_ your *mine*craft servers without any setup other than running one executable.
(People who join your server don't need to download anything!)

## Basic Usage:

By default, the mineshare app will use the public server. If you want to connect to
a custom server, see the `--help` menu of the app.

#### Note:

It is __highly__ recommended that you setup a whitelist for your server, as strangers are able to join your server if they find out the url!

#### Installation:

<details>
<summary>Download the appropriate mineshare-&ltarchitecture&gt executable
   from the GitHub releases or via another method.</summary>

- If you are on Windows, you will probably want `mineshare-x86_64-pc-windows-msvc.exe` 
  - If you know you're on an ARM cpu, you should go for `mineshare-x86_64-pc-windows-msvc.exe`
- If you are on macOS, you will probably want `mineshare-aarch64-apple-darwin` executable.
  - If you're on _intel_ macOS, you will want `mineshare-x86_64-apple-darwin`
- If you are on linux, you probably know which one to get (They're both built with musl)
</details>


#### Setup the Minecraft server:

<details>
<summary>Setup the local Minecraft server you will be using and note down its port number.</summary>

- This can be your local LAN server, or a full fledged server.
- If you are hosting on LAN, the port will be displayed in the chat
- If you are hosting a full server, the port is likely 25565

</details>

#### Start the connection server:

Open the command line and run mineshare with the argument of ip:port.
   Use the full executable name `mineshare-<arch>` (ex: `mineshare-x86_64-pc-windows-msvc.exe`).

```BASH
$ mineshare-<arch> <ip-address>:<port>
```
If you are hosting the server on your computer, you will want to use
     `localhost` as your ip.

The `--request-domain` or `-r` flag allows you to request a certain URL to be assigned.

```BASH
$ mineshare-<arch> localhost:25565 -r ThisIsTheDomainNameToRequest.mineshare.dev
``` 
<details>
<summary> Examples: </summary>
Hosting on your own computer with port 25565:

```BASH
$ mineshare-<arch> localhost:25565
```
<br>
Hosting on another computer that has IP 192.168.1.60 on port 5930:

```BASH
$ mineshare-<arch> 192.168.1.60:5930
```
</details>
<br>


After starting mineshare, you should see something like this:

```
<a few lines of details about connecting>
Proxy url: <word>-<word>-<word>.mineshare.dev
```

#### Connect to the server:

Both you and your friends can use that URL (`<word>-<word>-<word>.mineshare.dev`) to connect to the server in minecraft. Just type it into direct connect and join the server



#### Shutting down the connection:

Once you exit the mineshare application, all connected players will disconnect. No other work is required.


----

## Why shouldn't I use X instead?

By all means, go ahead! I just wanted an open source, publicly available server for people who don't want to
sign up or go through the effort of setting up a proxy just to play Minecraft with a few friends.


That being said, here's a few specific comparisons (to the extent of the knowledge I have of them):
(these comparisons use free tier as a comparison, because mineshare is also free)

<details>
<summary>
<a href="https://github.com/vgskye/e4mc-minecraft-architectury/">e4mc</a>
</summary>

- mineshare can proxy both LAN and regular servers, whereas e4mc is designed for LAN only (as far as I can tell)
- mineshare, since it is not a Minecraft mod, can work across versions (same mineshare executable works for 1.8.x to 1.21.x)
- e4mc is simpler to use
  - for mineshare, you need to know the ip and port of your server, and need to run the application separately
  - for e4mc, you just install the mod, open a LAN server and that's it

</details>

<details>
<summary>
<a href="https://ngrok.com/">ngrok</a>
</summary>

- mineshare is open source
- mineshare requires no signup
- mineshare has no total data transfer limit (it does have a bandwidth limit)
- ngrok supports custom firewall rules

</details>

<details>
<summary>
<a href="https://playit.gg/">playit.gg</a>
</summary>

- mineshare is open source
- mineshare requires no signup
- playit.gg supports custom firewall rules

</details>

If you notice any issues with these comparisons, let me know by creating an issue.

## Public server

#### Please do not abuse the public proxy server for unintended uses

The public server at `mineshare.dev` is a single, pretty weak server located in US-WEST.
It is rate-limited, but it may go down if people are using it too much.
You may get bad ping or multiple disconnects if you wish to use the public server.
I might work on getting a more powerful server/multiple servers in the future, but for now, that's it.

## Self hosting the proxy server

Self hosting the server is pretty easy. You just need to setup 1 DNS records, point them at your server,
open a few ports and people will be able to use it.

Github releases dont contain a binary build for the server, but CI builds them every commit and you can download from the
artifacts or compile it on your own.

You need to decide on a "base domain", which is the `mineshare.dev` in word-word-word.mineshare.dev,
and a "prefix", which is the prefix of the domain that the server will connect to. I recommend using "mc" for the prefix,
which is already set as the default (This means the server will connect to the proxy using the url `mc.<your base domain>`)
then you need to proxy anything from `*.<your base domain>`, in this case `*.mineshare.dev`, to your
server.

Then you open the ports `25563`, `25564`, and `25565`.

Then run the server.
There are also some other utilities in the CLI arguments if you would like to modify some things.

## How does it work (Technical details)

Proxy = mineshare_server*[.EXE]
Server = mineshare*[.EXE]

The proxy starts 3 TCP listeners.

1. Listener for the initial server connection
2. Listener for client
3. Listener for server "PLAY" requests

When a server wants to be proxied, it will connect to the initial server connection listener on the proxy.
The proxy server assigns a 3-word randomized id, or requested domain if available, to the server
from the [EFF large word list](https://www.eff.org/files/2016/07/18/eff_large_wordlist.txt) with 7776 words.
Since there's 3 words, it is highly unlikely for any malicious users to guess a server id.
Then the proxy sends this to the server, which will display it.

Once a client connects to the proxy with this server ID, the proxy parses the Minecraft "hello" packet for the hostname.
Using this hostname, the client is matched to the correct server, and the server is sent a client ID,
which is a 128-bit randomly generated number.
The server then initiates another TCP request to the proxy, this time on the "play" port (default 25564).

The proxy then connects the client stream and the server stream.
The server also creates a connection to the MC server.

At this point, the proxy and the server both transparently shuttle bytes back and forth between the client and the Minecraft server.

## Contributions

Are welcome!

(Except for contributions that are AI-generated. Please don't.)
