const { Keys, Client, NostrSigner, Nip46Signer, NostrConnectURI, loadWasmAsync, initLogger, LogLevel, Duration } = require("../");

async function main() {
    await loadWasmAsync();

    initLogger(LogLevel.info());

    // App keys
    let appKeys = Keys.parse("...");

    // Remote signer (NIP46)
    let uri = NostrConnectURI.parse("bunker://..");
    let timeout = Duration.fromSecs(60);
    let nip46 = await Nip46Signer.init(uri, appKeys, timeout);
    let signer = NostrSigner.nip46(nip46);

    // Compose client and add relays
    let client = new Client(signer);
    await client.addRelay("wss://relay.damus.io");
    await client.addRelay("wss://nos.lol");
    await client.addRelay("wss://nostr.oxtr.dev");
    await client.connect();

    await client.publishTextNote("My first text note from rust-nostr WASM with NIP46 signer!", []);
}

main();
