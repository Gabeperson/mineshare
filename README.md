# mineshare

mineshare is a simple, no portforwarding proxy app for small Minecraft servers (1.8.x-1.21.x)

Essentially, it lets you _share_ your *mine*craft servers without much setup other than running one executable.

## Basic Usage:

By default, the mineshare app will use the public server. If you want to connect to
a custom server, see the `--help` menu of the app.

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
   Use the full executable name (ex: `mineshare-x86_64-pc-windows-msvc.exe`).

```BASH
$ mineshare-<arch> <ip-address>:<port>
```
If you are hosting the server on your computer, you will want to use
     `localhost` as your ip.
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
Starting proxy connection
Proxy connection completed
Fetching url
Fetched Url
Proxy url: <word>-<word>-<word>.mineshare.dev
```

#### Connect to the server:

Both you and your friends can use that URL (`<word>-<word>-<word>.mineshare.dev`) to connect to the server in minecraft. Just type it into direct connect and join the server



#### Shutting down the connection:

Once you exit the mineshare application, all connected players will disconnect. No other work is required.


----


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

Then you open the ports `443`, `25564`, and `25565`.

Then run the server with the base domain and email info (for LetsEncrypt), and it should start running.
There are also some other utilities in the CLI arguments if you would like to modify some things.

## How does it work (Technical details)

Proxy = mineshare_server*[.EXE]
Server = mineshare*[.EXE]

The proxy first generates a ed25519 keypair.
The proxy then starts 3 TCP listeners.

1. Listener for the initial server connection
2. Listener for client
3. Listener for server "PLAY" requests

When a server wants to be proxied, it will connect to the initial server connection listener on the proxy
using raw TCP TLS on port 443. The proxy server assigns a 3-word randomized id to the server
from the [EFF large word list](https://www.eff.org/files/2016/07/18/eff_large_wordlist.txt) with 7776 words.
Since there's 3 words, it is HIGHLY unlikely ($\frac{1}{7776^3}$ or ~$`2.13*10^{-10}\%`$ chance per guess) for any malicious users to guess a server id.
Then the proxy sends this to the server, which will display it.

Once a client connects to the proxy with this server ID, the proxy parses the "hello" packet for the hostname.
Using this hostname, the client is matched to the correct server, and the server is sent a client ID,
which is a 128-bit randomly generated number, and also the ed25519 verification key.
The server then initiates another TCP request to the server, this time on a custom port (default 25564), and performs
an authenticated diffie-hellman exchange using the ed25519 verification key from before.
Using this shared secret and aes-gcm-siv, it encrypts the 128-bit ID and sends it over.

By doing this, we can ensure that no MITM can happen. See the `dhauth` module in the `src/lib.rs` file for more details
(Is there an easier & better way of doing this? Probably. Oh well, it works)

The proxy then decrypts this aes-gcm-siv encrypted ID using its own shared secret and connects the client stream and the server stream.
The server also creates a connection to the MC server.

Now there is a duplex connection from the client to the MC server, and we can just bidirectionally copy bytes over.

## Contributions

Are welcome!
