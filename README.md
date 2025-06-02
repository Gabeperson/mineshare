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
   Proxy url: word-word-word.mineshare.dev
   ```

   The people you want connecting to your server can now use the url given to connect to your server.

5. Shutting down the connection
   - Once you exit the mineshare application, all connected players will disconnect.

## Public server

The public server at `mineshare.dev` is a pretty weak server. It may go down if people are using it too much.
Unfortunately I do not have enough funds for a good one, so this is something you will have to accept if
you wish to use the public server.

## Self hosting the server

Self hosting the server is pretty easy. You just need to setup 1 DNS records, point them at your server,
and people will be able to use it.

You need to decide on a "base domain", which is the `mineshare.dev` in word-word-word.mineshare.dev,
then you need to proxy anything from `*.<your base domain>`, in this case `*.mineshare.dev`, to your
server.

Then run the server with the base domain and it should work.
There are also some other utilities in the CLI arguments if you would like to modify some things.

## Contributions

Are welcome!
