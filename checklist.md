# Validation Checklist

## High Priority
- [x] WebRTC -> SIP call still completes normally
- [x] SIP -> WebRTC call still completes normally
- [x] Caller hangup works both directions
- [x] Callee hangup works both directions
- [x] No misleading teardown warnings after hangup
- [x] Browser-facing WebRTC `200 OK` SDP now includes `telephone-event`
- [x] Bridge log no longer shows `target_dtmf=[]` on the WebRTC leg for SIP-phone DTMF
- [x] SIP phone -> WebRTC DTMF now sounds/behaves correctly
- [ ] Mixed recording still sounds correct on a normal bridged call

## Medium Priority
- [ ] `Trickle ICE` off: measure call start delay
- [ ] `Trickle ICE` on: measure call start delay
- [ ] Compare "click call" -> "INVITE sent" time with and without trickle
- [ ] Registration remains stable across page reload / reconnect
- [ ] Local extension dial works with `1002`
- [ ] Local extension dial works with `sip:1002@localhost`
- [ ] Direct trunk-style dial works with `sip:user@192.168.3.7`
- [ ] Ambiguous target `sip:192.168.3.7` behavior is confirmed/documented

## Asterisk / Controlled Tests
- [ ] Early media test extension installed
- [ ] `183 Session Progress` with SDP is sent before `200 OK`
- [ ] Caller hears early media before answer
- [ ] Ringback mode behavior matches expectation
- [ ] Post-answer DTMF collection test extension installed
- [ ] WebRTC -> Asterisk DTMF is detected correctly
- [ ] SIP phone -> Asterisk DTMF is detected correctly

## RWI Priority
- [ ] Java originate example still works
- [ ] Java bridge example still works
- [ ] One bridged leg hangup triggers peer hangup
- [ ] Attached inbound leg hangup still sends real SIP BYE
- [ ] RWI disconnect cleanup still behaves correctly

## Lower Priority / Cleanup
- [ ] Review duplicate DTMF codec merge logic for consolidation
- [ ] Review `MediaProxyMode::Nat` stub behavior
- [ ] Review bridge/runtime large parameter lists
- [ ] Review session timer edge cases after call-flow tests
