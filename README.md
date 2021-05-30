# Splinter Proxy

The Splinter proxy maps and redirects packets across multiple servers to give the appearance to a connected client of a single seamless Minecraft world.

The Splinter project aims to solve the problem that a single Minecraft world's performance is limited by the single most powerful computer that can be obtained. This project aims to fix this by splitting a single Minecraft world up into several servers such that this performance limit comes from the amount of hardware you have.

## Usage

### Building Splinter Proxy

You will need Rust. You can get this through [rustup](https://rustup.rs).

You need to be on the nightly branch for certain features. `rustup default nightly`

Build and run through `cargo run`

### Setting up Minecraft server

Grab a 1.16.5 server from [Spigot BuildTools](https://www.spigotmc.org/wiki/buildtools) or [Paper](https://papermc.io/downloads).

There are some required settings in server.properties:

- `server-port=25400` This can be changed to whatever you set for a server address in the generated `config.ron`.
- `online-mode=false` to disable authentication, as authentication will be done in a proxy on top of the server like BungeeCord.
 
Then you can run the server with `java -jar [server jar name].jar --nogui` or run it as a normal application given you have Java installed.

### Joining the proxy

Join with a 1.16.5 Minecraft client. If you're running the proxy on the same device you're playing from, then you can connect to `localhost:25565`.

## Contributing

Join the [OpenClique](https://discord.gg/F93NMyBHda) discord.

Check out the [prototype](https://github.com/OpenCliqueCraft/splinter-prototype).

Contact:

- Discord: [Leap#0765](https://gardna.net/discord)
- Discord: [regenerativep#4103](https://discord.com/users/198652932802084864)

