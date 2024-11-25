// This file is automatically generated, so please do not edit it.
// @generated by `flutter_rust_bridge`@ 2.6.0.

// ignore_for_file: invalid_use_of_internal_member, unused_import, unnecessary_import

import '../../frb_generated.dart';
import 'event/tag.dart';
import 'key/public_key.dart';
import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated.dart';

// Rust type: RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Event>>
abstract class Event implements RustOpaqueInterface {
  Future<String> asJson();

  Future<String> asPrettyJson();

  /// Get event author (`pubkey` field)
  Future<PublicKey> author();

  Future<String> content();

  Future<BigInt> createdAt();

  static Event fromJson({required String json}) =>
      RustLib.instance.api.crateApiProtocolEventEventFromJson(json: json);

  Future<String> id();

  /// Returns `true` if the event has an expiration tag that is expired.
  /// If an event has no expiration tag, then it will return `false`.
  ///
  /// <https://github.com/nostr-protocol/nips/blob/master/40.md>
  Future<bool> isExpired();

  /// Check if it's a protected event
  ///
  /// <https://github.com/nostr-protocol/nips/blob/master/70.md>
  Future<bool> isProtected();

  Future<int> kind();

  Future<String> signature();

  Future<List<Tag>> tags();

  /// Verify both `EventId` and `Signature`
  Future<void> verify();

  /// Verify if the `EventId` it's composed correctly
  Future<bool> verifyId();

  /// Verify only event `Signature`
  Future<bool> verifySignature();
}
