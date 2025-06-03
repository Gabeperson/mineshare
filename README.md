# mineshare

### Please do not abuse the public proxy server for unintended uses

mineshare is a simple, no portforwarding proxy app for small Minecraft servers

Essentially, it lets you _share_ your *mine*craft servers without much setup other than running one executable.

## Usage:

By default, the mineshare app will use the public server. If you want to connect to
a custom server, see the --help menu of the app.

1. Download the `mineshare` executable from the GitHub releases or via another method.
2. Setup the local server you will be using and note down its port number.
   - This can be your local LAN server, or a full fledged server.
   - If you are hosting on LAN, the port will be displayed in the chat
   - If you are hosting a full server, the port is likely 25565
3. Open the command line and run mineshare with the argument of ip:port.

   - If you are hosting the server on your computer, you will want to use
     `localhost` as your ip.
   - Examples:

     - Hosting on your own computer with port 25565:
       `mineshare localhost:25565`
     - Hosting on your own computer with port 60000:
       `mineshare localhost:60000`
     - Hosting on another computer that has IP 192.168.1.60 and port 5930:
       `mineshare 192.168.1.60:5930`

4. You should see something like:

   ```
   Starting proxy connection
   Proxy connection completed
   Fetching url
   Fetched Url
   Proxy url: a_word-a_word-a_word.mineshare.dev
   ```

   The people you want connecting to your server can now use the url given to connect to your server.

5. Shutting down the connection
   - Once you exit the mineshare application, all connected players will disconnect.

## Public server

The public server at `mineshare.dev` is a pretty weak server located in US-WEST.
It is rate-limited, but it may go down if people are using it too much.
Unfortunately I do not have enough funds for a good one, nor to host multiple in multiple regions, so you may get
bad pings or multiple disconnects if you wish to use the public server.

## Self hosting the proxy server

Self hosting the server is pretty easy. You just need to setup 1 DNS records, point them at your server,
open a few ports and people will be able to use it.

You need to decide on a "base domain", which is the `mineshare.dev` in word-word-word.mineshare.dev,
and a "prefix", which is the prefix of the domain that the server will connect to. I recommend using "mc" for the prefix,
which is already set as the default (This means the server will connect to the proxy using the url `mc.<your base domain>`)
then you need to proxy anything from `*.<your base domain>`, in this case `*.mineshare.dev`, to your
server.

Then you open the ports `443`, `25564`, and `25565`.

Then run the server with the base domain and email info (for LetsEncrypt), and it should start running.
There are also some other utilities in the CLI arguments if you would like to modify some things.

## How does it work

The server first starts 3 listeners.

1. Listener for the initial server connection
2. Listener for client
3. Listener for server "PLAY" requests

When a server wants to be proxied, it will connect to the initial server connection listener on the proxy server
using raw TCP TLS on port 443. The proxy server assigns a 3-word randomized id to the server
from the [EFF large word list](https://www.eff.org/files/2016/07/18/eff_large_wordlist.txt) with 7776 words.
Since there's 3 words, it is HIGHLY unlikely ($1/7776^3$ or $~2.13e-10%$) for any malicious users to guess a server id.
Then the proxy server sends this to the server, which will display it.

Once a client connects with this server ID, it is matched to the correct server, and the server is sent a client ID, which is a 128-bit randomly generated number.
The server then initiates another TCP TLS request to the server, again on 443, then sends the client ID.
The server also creates a connection to the MC server.
Once the ID is sent, both the server and client cancel the TLS, and they begin proxying their respective connections:

- The proxy looks up the ID, and connects the waiting client with the server connection (now non-tls)
- The server connects the now non-tls'd connection to the proxy to the actual MC server stream.

## Contributions

Are welcome!
