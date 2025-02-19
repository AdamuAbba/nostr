package rust.nostr.snippets

// ANCHOR: full
import kotlinx.coroutines.runBlocking
import rust.nostr.sdk.*

suspend fun hello() {
    // ANCHOR: client
    val keys = Keys.generate()
    val signer = NostrSigner.keys(keys)
    val client = Client(signer = signer)
    // ANCHOR_END: client

    // ANCHOR: connect
    client.addRelay("wss://relay.damus.io")
    client.connect()
    // ANCHOR_END: connect

    // ANCHOR: publish
    val builder = EventBuilder.textNote("Hello, rust-nostr!")
    val output = client.sendEventBuilder(builder)
    // ANCHOR_END: publish

    // ANCHOR: output
    println("Event ID: ${output.id.toBech32()}")
    println("Sent to: ${output.success}")
    println("Not sent to: ${output.failed}")
    // ANCHOR_END: output
}

fun main() {
    runBlocking { hello() }
}
// ANCHOR_END: full
