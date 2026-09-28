#!/usr/bin/env python3
"""Shared-transcode-dir drill client (transcode/docs/SHARED-TRANSCODE.md).

    JM_TOKEN=<api key> shared_dir_drill.py <url> --title SUBSTR [--sessions 3] [--seconds 150]
        [--host HOST] [--bitrate 3000000] [--max-height 720]
        [--pause-at 40 --pause-for 150 --ping-url URL] [--events FILE]
        [--start-offset 1200] [--seek-at 90 --seek-by 300]

Opens N independent HLS transcoding sessions (distinct DeviceId + PlaySessionId) through one URL
(e.g. the Traefik failover route) and pulls segments at real-time pace with a 12 s buffer, like a
player. Every 10 s each session POSTs /Sessions/Playing/Progress (IsPaused as appropriate) -- to
--ping-url when given, which lets a drill land the pings on a different replica than the one that
owns the job. --pause-at/--pause-for make session 0 stop fetching for a while (paused pings only).
--start-offset starts session i that far into the title (disjoint segment ranges per session, so
a server-side view of the outputs can be told apart); --seek-at/--seek-by make session 0 jump
forward like a player seek (the next request is a segment far past anything written: a restart).

Prints one line per slow (> --slow, default 3 s) or failed segment, and a RESULT line per session
plus a TOTAL line. Every request is also appended to --events as JSON lines (time, session,
segment, seconds, status, the replica that answered if the server says so) for later analysis.
"""
import argparse
import json
import os
import re
import threading
import time
import uuid

import requests
import urllib3

urllib3.disable_warnings()

ap = argparse.ArgumentParser()
ap.add_argument('url')
ap.add_argument('--title', required=True)
ap.add_argument('--sessions', type=int, default=3)
ap.add_argument('--seconds', type=float, default=150)
ap.add_argument('--bitrate', type=int, default=3_000_000)
ap.add_argument('--max-height', type=int, default=720)
ap.add_argument('--host', help='Host header (reaching Traefik by address); disables TLS verification')
ap.add_argument('--slow', type=float, default=3.0)
ap.add_argument('--start-offset', type=float, default=0,
                help='session i starts playing at i x this many seconds into the title (a segment of the playlist)')
ap.add_argument('--seek-at', type=float, default=-1, help='session 0 seeks at this many seconds into the drill')
ap.add_argument('--seek-by', type=float, default=300, help='... this many seconds forward')
ap.add_argument('--pause-at', type=float, default=-1)
ap.add_argument('--pause-for', type=float, default=0)
ap.add_argument('--ping-url', help='send progress pings here instead of <url>')
ap.add_argument('--events', default='')
a = ap.parse_args()
key = os.environ['JM_TOKEN']
lock = threading.Lock()
evf = open(a.events, 'a') if a.events else None
T0 = time.time()


def event(**kw):
    kw['t'] = round(time.time() - T0, 3)
    if evf:
        with lock:
            evf.write(json.dumps(kw) + '\n')
            evf.flush()


def headers(dev):
    h = {'Host': a.host} if a.host else {}
    h['Authorization'] = (f'MediaBrowser Client="shared-dir-drill", Device="shared-dir-drill", '
                          f'DeviceId="{dev}", Version="1.0", Token="{key}"')
    return h


def pick_movie():
    r = requests.get(a.url + '/Items', headers=headers('drill-setup'), timeout=60, verify=not a.host,
                     params={'Recursive': 'true', 'IncludeItemTypes': 'Movie', 'Fields': 'MediaSources'})
    r.raise_for_status()
    return next(m for m in r.json()['Items'] if a.title.lower() in m['Name'].lower())


class Session(threading.Thread):
    def __init__(self, idx, movie):
        super().__init__(daemon=True)
        self.idx, self.movie = idx, movie
        self.dev = f'drill-{uuid.uuid4().hex[:8]}'
        self.ps = uuid.uuid4().hex
        self.H = headers(self.dev)
        self.stats = dict(segments=0, failed=0, slow=0, slowest=0.0, lowest_buffer=99.0, cold=[])
        self.paused = False
        self.position = 0.0

    def get(self, path, **params):
        return requests.get(a.url + path, headers=self.H, params=params or None, timeout=60, verify=not a.host)

    def ping(self):
        body = {'ItemId': self.movie['Id'], 'MediaSourceId': self.movie['MediaSources'][0]['Id'],
                'PlaySessionId': self.ps, 'PositionTicks': int(self.position * 1e7),
                'IsPaused': self.paused, 'PlayMethod': 'Transcode', 'CanSeek': True}
        url = a.ping_url or a.url
        try:
            r = requests.post(url + '/Sessions/Playing/Progress', headers=self.H, json=body, timeout=30,
                              verify=not a.host)
            event(kind='ping', s=self.idx, status=r.status_code, paused=self.paused, via=url)
        except requests.RequestException as e:
            event(kind='ping', s=self.idx, status=type(e).__name__, paused=self.paused, via=url)

    def run(self):
        base = f"/Videos/{self.movie['Id']}/"
        r = self.get(base + 'master.m3u8', MediaSourceId=self.movie['MediaSources'][0]['Id'], VideoCodec='h264',
                     AudioCodec='aac', SegmentContainer='ts', MaxStreamingBitrate=a.bitrate,
                     VideoBitrate=a.bitrate - 192000, AudioBitrate=192000, MaxHeight=a.max_height,
                     TranscodingMaxAudioChannels=2, PlaySessionId=self.ps, DeviceId=self.dev, api_key=key)
        r.raise_for_status()
        variant = next(line for line in r.text.splitlines() if line and not line.startswith('#'))
        r = self.get(base + variant)
        r.raise_for_status()
        durs = [float(x) for x in re.findall(r'#EXTINF:([\d.]+)', r.text)]
        segs = [line for line in r.text.splitlines() if line and not line.startswith('#')]
        try:
            requests.post(a.url + '/Sessions/Playing', headers=self.H, timeout=30, verify=not a.host,
                          json={'ItemId': self.movie['Id'], 'PlaySessionId': self.ps, 'PlayMethod': 'Transcode'})
        except requests.RequestException:
            pass
        print(f"[s{self.idx}] '{self.movie['Name']}' {len(segs)} segs, session {self.ps[:8]}", flush=True)
        t0 = time.monotonic()
        have, i, last_ping, paused_total = 0.0, 0, 0.0, 0.0
        seeked = False
        cold = True

        def jump(to_secs):
            # Position the player at the segment holding to_secs, as if it had just started there.
            nonlocal have, i, paused_total
            j, acc = 0, 0.0
            while j < len(segs) - 1 and acc + durs[j] <= to_secs:
                acc += durs[j]
                j += 1
            have, i = acc, j
            paused_total = (time.monotonic() - t0) - acc
            self.position = acc

        if a.start_offset and self.idx:
            jump(a.start_offset * self.idx)
            print(f'[s{self.idx}] starts at segment {i}', flush=True)
        while i < len(segs) and time.monotonic() - t0 < a.seconds:
            now = time.monotonic() - t0
            if self.idx == 0 and a.seek_at >= 0 and now >= a.seek_at and not seeked:
                seeked = True
                old = i
                jump(have + a.seek_by)
                cold = True
                event(kind='seek', s=self.idx, frm=old, to=i)
                print(f'[s{self.idx}] +{now:5.1f}s SEEK segment {old} -> {i}', flush=True)
                self.ping()
                last_ping = now
            if self.idx == 0 and a.pause_at >= 0 and a.pause_at <= now < a.pause_at + a.pause_for:
                if not self.paused:
                    self.paused = True
                    print(f'[s{self.idx}] +{now:5.1f}s PAUSE for {a.pause_for:.0f}s', flush=True)
                    self.ping()
                    last_ping = now
            elif self.paused:
                self.paused = False
                paused_total += a.pause_for
                print(f'[s{self.idx}] +{now:5.1f}s RESUME', flush=True)
                self.ping()
                last_ping = now
            if now - last_ping >= 10:
                self.ping()
                last_ping = now
            self.position = min(have, now - paused_total) if not self.paused else self.position
            if self.paused or have - (now - paused_total) > 12:
                time.sleep(0.2)
                continue
            ts = time.monotonic()
            try:
                r = self.get(base + segs[i])
                ok, why = r.status_code == 200 and len(r.content) > 1000, f'HTTP {r.status_code}'
                via = r.headers.get('X-Jellyfin-Node', '')
            except requests.RequestException as e:
                ok, why, via = False, type(e).__name__, ''
            took = time.monotonic() - ts
            event(kind='seg', s=self.idx, seg=i, took=round(took, 3), ok=ok, why=why, via=via, cold=cold)
            if not ok:
                self.stats['failed'] += 1
                print(f'[s{self.idx}] +{time.monotonic() - t0:5.1f}s seg {i} FAILED ({why}); retry', flush=True)
                time.sleep(1)
                continue
            self.stats['segments'] += 1
            if cold:
                # The first segment of a start or a seek is a cold start (a new transcode):
                # reported on its own, not counted as slow.
                cold = False
                self.stats['cold'].append(round(took, 2))
                if seeked:
                    print(f'[s{self.idx}] first segment after the seek ({i}) took {took:.2f}s', flush=True)
            else:
                buf = have - (time.monotonic() - t0 - paused_total)
                self.stats['lowest_buffer'] = min(self.stats['lowest_buffer'], buf)
                self.stats['slowest'] = max(self.stats['slowest'], took)
                if took > a.slow:
                    self.stats['slow'] += 1
                    print(f'[s{self.idx}] +{time.monotonic() - t0:5.1f}s seg {i} SLOW {took:.2f}s '
                          f'(buffer {buf:.1f}s)', flush=True)
            have += durs[i]
            i += 1
        try:
            requests.post(a.url + '/Sessions/Playing/Stopped', headers=self.H, timeout=30, verify=not a.host,
                          json={'ItemId': self.movie['Id'], 'PlaySessionId': self.ps,
                                'PositionTicks': int(self.position * 1e7)})
        except requests.RequestException:
            pass
        s = self.stats
        print(f"RESULT s{self.idx} session={self.ps[:8]} segments={s['segments']} failed={s['failed']} "
              f"slow={s['slow']} slowest={s['slowest']:.2f}s lowest_buffer={s['lowest_buffer']:.1f}s "
              f"cold_starts={s['cold']}", flush=True)


movie = pick_movie()
sessions = [Session(i, movie) for i in range(a.sessions)]
for s in sessions:
    s.start()
    time.sleep(1)
for s in sessions:
    s.join()
tot = {k: sum(s.stats[k] for s in sessions) for k in ('segments', 'failed', 'slow')}
print(f"TOTAL sessions={len(sessions)} segments={tot['segments']} failed={tot['failed']} slow={tot['slow']} "
      f"slowest={max(s.stats['slowest'] for s in sessions):.2f}s "
      f"lowest_buffer={min(s.stats['lowest_buffer'] for s in sessions):.1f}s", flush=True)
