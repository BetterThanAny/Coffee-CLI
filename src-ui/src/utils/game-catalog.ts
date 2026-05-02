/** A single trusted game entry, localized for the active UI language. */
export interface RemoteGameEntry {
  id: string;
  file: string;
  title: string;
  icon: string;
  download: string;
  sha256: string;
  dosbox_conf?: string;
}

interface RemoteCatalogEntry {
  id?: string;
  title?: string;
}

interface GameCatalogJson {
  version: number;
  catalogs?: Record<string, RemoteCatalogEntry[]>;
}

const CATALOG_URL = 'https://coffeecli.com/play/game.json';
const ASSET_BASE_URL = 'https://coffeecli.com/play';

const DOOM_CONF = `[sdl]
autolock=false

[dosbox]
machine=vga

[cpu]
core=auto
cputype=auto
cycles=fixed 50000

[mixer]
nosound=false
rate=44100
blocksize=1024
prebuffer=20

[sblaster]
sbtype=sb16
sbbase=220
irq=5
dma=1
hdma=5
sbmixer=true
oplmode=auto
oplemu=default
oplrate=44100

[autoexec]
echo off
SET BLASTER=A220 I5 D1 H5 T6
mount c .
c:
DOOM.EXE`;

const PRINCE_CONF = `[sdl]
autolock=false

[dosbox]
machine=svga_s3
memsize=16

[cpu]
core=auto
cputype=auto
cycles=auto

[mixer]
nosound=false
rate=44100
blocksize=1024
prebuffer=20

[autoexec]
echo off
mount c .
c:
prince`;

const PAL_CONF = `[sdl]
autolock=false

[dosbox]
machine=svga_s3
memsize=16

[cpu]
core=auto
cputype=auto
cycles=auto

[mixer]
nosound=false
rate=44100
blocksize=1024
prebuffer=20

[autoexec]
echo off
mount c .
c:
PAL!.EXE`;

const STARDOM_CONF = `[sdl]
autolock=false

[dosbox]
machine=svga_s3
memsize=16

[cpu]
core=auto
cputype=auto
cycles=auto

[mixer]
nosound=false
rate=44100
blocksize=1024
prebuffer=20

[autoexec]
echo off
mount c .
c:
STARDOM.EXE`;

const TRUSTED_GAMES: RemoteGameEntry[] = [
  {
    id: 'doom',
    file: 'doom.jsdos',
    title: 'DOOM',
    icon: `${ASSET_BASE_URL}/icons/doom.png`,
    download: `${ASSET_BASE_URL}/doom.jsdos`,
    sha256: 'e7d9d83d861820b07fc55906db5d9b2997444c0b6e6e1339d3f390a2ec3c5767',
    dosbox_conf: DOOM_CONF,
  },
  {
    id: 'prince-of-persia',
    file: 'prince-of-persia.jsdos',
    title: 'Prince of Persia',
    icon: `${ASSET_BASE_URL}/icons/prince-of-persia.png`,
    download: `${ASSET_BASE_URL}/prince-of-persia.jsdos`,
    sha256: 'e1803db94d092c68acc6928adcde11c8bd6b88b95d71b1f06954a62bfb17c6dc',
    dosbox_conf: PRINCE_CONF,
  },
  {
    id: 'pal',
    file: 'pal.jsdos',
    title: 'Chinese Paladin',
    icon: `${ASSET_BASE_URL}/icons/pal.jpg`,
    download: `${ASSET_BASE_URL}/pal.jsdos`,
    sha256: '48a58be784ad5b08135a4b0f9fc18a32b42c827f0901ddf7787422556cff64da',
    dosbox_conf: PAL_CONF,
  },
  {
    id: 'stardom',
    file: 'stardom.jsdos',
    title: 'Stardom',
    icon: `${ASSET_BASE_URL}/icons/stardom.webp`,
    download: `${ASSET_BASE_URL}/stardom.jsdos`,
    sha256: '474c470fd4b5de4cd14fb96c87e9626d8a77cc90b5c14cf1e04cdbb9bc04edb8',
    dosbox_conf: STARDOM_CONF,
  },
];

let _cache: GameCatalogJson | null = null;
let _inflight: Promise<GameCatalogJson> | null = null;

function fetchCatalogJson(): Promise<GameCatalogJson> {
  if (_cache) return Promise.resolve(_cache);
  if (!_inflight) {
    _inflight = fetch(CATALOG_URL)
      .then(r => { if (!r.ok) throw new Error(String(r.status)); return r.json(); })
      .then((d: GameCatalogJson) => { _cache = d; return _cache; })
      .catch(() => ({ version: 0, catalogs: {} }));
  }
  return _inflight;
}

function remoteEntriesForLang(json: GameCatalogJson, lang: string): RemoteCatalogEntry[] {
  const catalogs = json.catalogs ?? {};
  return catalogs[lang]
    ?? catalogs[lang.split('-')[0]]
    ?? catalogs.default
    ?? [];
}

function safeRemoteTitle(entry: RemoteCatalogEntry | undefined): string | undefined {
  if (!entry || typeof entry.title !== 'string') return undefined;
  const title = entry.title.trim();
  if (title.length === 0 || title.length > 80) return undefined;
  return title;
}

/**
 * Returns the trusted game list for the given BCP-47 language tag.
 * The remote catalog is allowed to localize display titles only. Download
 * URLs, filenames, hashes, icons, and DOSBox config stay pinned in code.
 */
export async function fetchGameCatalog(lang: string): Promise<RemoteGameEntry[]> {
  const json = await fetchCatalogJson();
  const remoteById = new Map(remoteEntriesForLang(json, lang).map(entry => [entry.id, entry]));

  return TRUSTED_GAMES.map(game => ({
    ...game,
    title: safeRemoteTitle(remoteById.get(game.id)) ?? game.title,
  }));
}
