"""python3 -m unittest discover -s crates/compositor/tests -p test_bench.py"""
import struct
import unittest

from bench import RtpUnits, frame_metrics


def packet(sequence, payload, timestamp=9000, marker=False, generation=1):
    return struct.pack('!BBHII', 128, 96 | (128 if marker else 0), sequence, timestamp, generation) + payload


def stap(*nals):
    return b'\x78' + b''.join(struct.pack('!H', len(n)) + n for n in nals)


AUD, IDR, DELTA = b'\x09\xf0', b'\x65\x11\x23', b'\x41\x31'


class RtpTests(unittest.TestCase):
    def test_fragmented_unit_and_sequence_wrap(self):
        r = RtpUnits()
        r.consume(packet(65534, AUD), 1)
        r.consume(packet(65535, b'\x7c\x85\x11'), 2)
        r.consume(packet(0, b'\x7c\x05\x23'), 3)
        self.assertEqual(r.units, [])
        r.consume(packet(1, b'\x7c\x45\x37', marker=True), 4)
        self.assertEqual(r.units, [(1, 9000, 4, True)])
        self.assertEqual(r.sample, b'\0\0\0\1' + AUD + b'\0\0\0\1\x65\x11\x23\x37')
        self.assertFalse(any(r.errors.values()))

    def test_missing_middle_packet_rejects_marker_and_resynchronizes(self):
        r = RtpUnits()
        r.consume(packet(10, AUD), 0)
        r.consume(packet(11, b'\x7c\x85\x11'), 0)
        r.consume(packet(13, b'\x7c\x45\x37', marker=True), 0)
        self.assertEqual(r.units, [])
        self.assertEqual(r.errors['missing_packets'], 1)
        self.assertEqual(r.errors['partial_units'], 1)
        r.consume(packet(14, stap(AUD, DELTA), timestamp=10500, marker=True), 1)
        self.assertEqual(r.units, [(1, 10500, 1, False)])

    def test_duplicate_marker_is_not_a_second_unit(self):
        r = RtpUnits()
        value = packet(2, stap(AUD, IDR), marker=True)
        r.consume(value, 1)
        r.consume(value, 2)
        self.assertEqual(len(r.units), 1)
        self.assertEqual(r.errors['duplicate_packets'], 1)

    def test_reordering_is_reported_without_repair_or_extra_count(self):
        r = RtpUnits()
        r.consume(packet(10, AUD), 0)
        r.consume(packet(12, IDR, marker=True), 1)
        r.consume(packet(11, b'\x06\x01'), 2)
        self.assertEqual(r.units, [])
        self.assertEqual(r.errors['reordered_packets'], 1)
        self.assertEqual(r.errors['missing_packets'], 1)

    def test_no_aud_or_open_fragment_cannot_be_completed_by_marker(self):
        for payload in [IDR, b'\x7c\x45\x22', b'\x7c\xc5\x22']:
            r = RtpUnits()
            r.consume(packet(1, payload, marker=True), 0)
            self.assertEqual(r.units, [])
        r = RtpUnits()
        r.consume(packet(1, AUD), 0)
        r.consume(packet(2, b'\x7c\x85\x11', marker=True), 1)
        self.assertEqual(r.units, [])
        self.assertEqual(r.errors['partial_units'], 1)

    def test_missing_marker_and_final_partial_are_reported(self):
        r = RtpUnits()
        r.consume(packet(1, stap(AUD, IDR)), 0)
        r.consume(packet(2, stap(AUD, DELTA), timestamp=10500, marker=True), 1)
        r.consume(packet(3, AUD, timestamp=12000), 2)
        r.abandon()
        self.assertEqual(r.errors['partial_units'], 2)
        self.assertEqual(r.units, [(1, 10500, 1, False)])

    def test_malformed_stap_and_header_are_rejected(self):
        for value in [b'bad', packet(1, b'\x78\x00\x05\x65', marker=True),
                      packet(1, b'\x78\x00', marker=True), packet(1, b'\x78', marker=True)]:
            r = RtpUnits()
            r.consume(value, 0)
            self.assertEqual(r.units, [])
            self.assertEqual(r.errors['invalid_packets'], 1)

    def test_generation_change_does_not_join_fragments(self):
        r = RtpUnits()
        r.consume(packet(1, AUD), 0)
        r.consume(packet(2, b'\x7c\x85\x11'), 0)
        r.consume(packet(1, b'\x7c\x45\x22', generation=2, marker=True), 1)
        r.consume(packet(3, IDR, marker=True), 2)
        self.assertEqual(r.units, [])
        self.assertEqual(r.errors['stale_packets'], 1)

    def test_timestamp_wrap_is_valid_but_regression_is_not(self):
        r = RtpUnits()
        for seq, timestamp in [(1, 0xffffff00), (2, 1244), (3, 1000)]:
            r.consume(packet(seq, stap(AUD, IDR), timestamp, marker=True), seq)
        self.assertEqual(len(r.units), 2)
        self.assertEqual(r.errors['regressed_timestamps'], 1)

    def test_missing_initial_key_does_not_shift_metadata_correlation(self):
        r = RtpUnits()
        r.consume(packet(10, stap(AUD, DELTA), marker=True), 10.5)
        frame = dict(generation=1, capture=10.2, received=10.3, sequence=0)
        m = frame_metrics([frame], r, 10, 11, 60)
        self.assertEqual(m['complete_encoded_units'], 1)
        self.assertEqual(m['unmatched_complete_units'], 1)
        self.assertEqual(m['capture_cohort_uncompleted_after_drain'], 1)

    def test_capture_cohort_is_not_completion_window_and_timestamp_wrap(self):
        # One warmup capture completes during the window; one window capture
        # completes after it; one is lost. Capture sequence gaps are NOT RTP gaps.
        anchor = 0xffffff00
        r = RtpUnits()
        r.anchors = {1: anchor}
        r.units = [(1, anchor, 10.1, True), (1, (anchor+1500)&0xffffffff, 11.2, False)]
        frames = [dict(generation=1, capture=c, sequence=s, received=t)
                  for c, s, t in [(9.9, 4, 10.0), (10.2, 7, 10.8), (10.7, 9, 11.1)]]
        m = frame_metrics(frames, r, 10, 11, 60)
        self.assertEqual(m['submitted_frames'], 2)
        self.assertEqual(m['complete_encoded_units'], 1)
        self.assertEqual(m['capture_cohort_completed_by_end'], 0)
        self.assertEqual(m['capture_cohort_completed_late'], 1)
        self.assertEqual(m['capture_cohort_uncompleted_after_drain'], 1)
        self.assertEqual(m['replaced_frames_observed_span'], 1)
        self.assertEqual(m['rendered_attempts_observed_span'], 3)

    def test_window_start_is_inclusive_and_end_is_exclusive(self):
        r = RtpUnits()
        r.anchors = {1: 0}
        r.units = [(1, 0, 10, True), (1, 1500, 11, False)]
        frames = [dict(generation=1, capture=t, received=t, sequence=i)
                  for i, t in enumerate([10, 11])]
        m = frame_metrics(frames, r, 10, 11, 60)
        self.assertEqual(m['submitted_frames'], 1)
        self.assertEqual(m['complete_encoded_units'], 1)
        self.assertEqual(m['capture_cohort_completed_by_end'], 1)


if __name__ == '__main__':
    unittest.main()
