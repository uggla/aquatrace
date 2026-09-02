import L from 'leaflet';

type MapStyle = 'opentopo' | 'openstreetmap';
type Coordinates = { lat: number; lon: number };
type Place = Coordinates & { osm_id: number; name: string; place_type: string };
type NearbyWaterPoint = Coordinates & {
  osm_type: 'node' | 'way';
  osm_id: number;
  name?: string;
  distance_m: number;
  street_view_url?: string;
};
type NearbyResponse = {
  center: Coordinates;
  radius_m: number;
  water_points: NearbyWaterPoint[];
  truncated: boolean;
};
type PlaceSearchResponse = { places: Place[] };
type StreetViewResponse = { locations: Array<{ id: string; street_view_url?: string }> };

const tileLayers: Record<MapStyle, { url: string; options: L.TileLayerOptions }> = {
  opentopo: {
    url: 'https://{s}.tile.opentopomap.org/{z}/{x}/{y}.png',
    options: { maxZoom: 17, attribution: '© OpenStreetMap contributors © OpenTopoMap' }
  },
  openstreetmap: {
    url: 'https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png',
    options: { maxZoom: 19, attribution: '© OpenStreetMap contributors' }
  }
};

const placeInput = mustQuery<HTMLInputElement>('#place-query');
const suggestions = mustQuery<HTMLUListElement>('#place-suggestions');
const statusMessage = mustQuery<HTMLParagraphElement>('#nearby-status-message');
const statusLine = statusMessage.closest<HTMLElement>('.status-line');
const locationSummary = mustQuery<HTMLParagraphElement>('#nearby-location-summary');
const waterCount = mustQuery<HTMLSpanElement>('#nearby-water-count');
const waterTable = mustQuery<HTMLTableSectionElement>('#nearby-water-table');
const radiusLabel = mustQuery<HTMLSpanElement>('#nearby-radius-label');
const mapStyleSelect = mustQuery<HTMLSelectElement>('#nearby-map-style');
const mapShell = mustQuery<HTMLDivElement>('#nearby-map-shell');
const mapEmpty = mustQuery<HTMLDivElement>('#nearby-map-empty');
const fullscreenButton = mustQuery<HTMLButtonElement>('#nearby-fullscreen-button');
const resetViewButton = mustQuery<HTMLButtonElement>('#nearby-reset-view-button');
const radiusButtons = Array.from(document.querySelectorAll<HTMLButtonElement>('[data-nearby-radius]'));

let selectedCenter: Coordinates | null = null;
let selectedLocationName = '';
let selectedRadius = 1_000;
let currentResponse: NearbyResponse | null = null;
let selectedWaterPointKey: string | null = null;
let searchTimer: number | undefined;
let activeSuggestion = -1;
let currentSuggestions: Place[] = [];
let placeController: AbortController | null = null;
let nearbyController: AbortController | null = null;
let streetViewController: AbortController | null = null;
let radiusCircle: L.Circle | null = null;
let centerMarker: L.CircleMarker | null = null;

const map = L.map('nearby-map', { scrollWheelZoom: true, zoomControl: false }).setView([48.5, 9], 4);
L.control.zoom({ position: 'topright' }).addTo(map);
let currentTileLayer = createTileLayer('opentopo').addTo(map);
const markerLayer = L.layerGroup().addTo(map);

map.on('click', (event) => {
  placeInput.value = '';
  chooseCenter({ lat: event.latlng.lat, lon: event.latlng.lng }, 'Selected location');
});

placeInput.addEventListener('input', () => {
  window.clearTimeout(searchTimer);
  const query = placeInput.value.trim();
  if (query.length < 3) {
    placeController?.abort();
    hideSuggestions();
    return;
  }
  searchTimer = window.setTimeout(() => void searchPlaces(query), 300);
});

placeInput.addEventListener('keydown', (event) => {
  if (suggestions.hidden || currentSuggestions.length === 0) return;
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    event.preventDefault();
    const direction = event.key === 'ArrowDown' ? 1 : -1;
    activeSuggestion = (activeSuggestion + direction + currentSuggestions.length) % currentSuggestions.length;
    renderSuggestions();
  } else if (event.key === 'Enter' && activeSuggestion >= 0) {
    event.preventDefault();
    selectPlace(currentSuggestions[activeSuggestion]);
  } else if (event.key === 'Escape') {
    hideSuggestions();
  }
});

document.addEventListener('click', (event) => {
  if (!(event.target instanceof Element) || !event.target.closest('.place-search-box')) hideSuggestions();
});

for (const button of radiusButtons) {
  button.addEventListener('click', () => {
    selectedRadius = Number(button.dataset.nearbyRadius);
    updateRadiusButtons();
    if (selectedCenter) {
      currentResponse = null;
      selectedWaterPointKey = null;
      waterCount.textContent = '0 water points';
      waterTable.innerHTML = '<tr><td colspan="4" class="empty-cell">Searching for drinking water...</td></tr>';
      renderSearchGeometry([], true);
      void loadNearby(true);
    }
  });
}

mapStyleSelect.addEventListener('change', () => setMapStyle(mapStyleSelect.value as MapStyle));
resetViewButton.addEventListener('click', fitSearchArea);
fullscreenButton.addEventListener('click', () => void toggleFullscreen());
document.addEventListener('fullscreenchange', () => {
  updateFullscreenButton();
  window.setTimeout(() => map.invalidateSize(), 100);
});

async function searchPlaces(query: string): Promise<void> {
  placeController?.abort();
  const controller = new AbortController();
  placeController = controller;
  try {
    const response = await fetch(`/api/places/search?q=${encodeURIComponent(query)}`, { signal: controller.signal });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const result = (await response.json()) as PlaceSearchResponse;
    if (controller.signal.aborted || placeInput.value.trim() !== query) return;
    currentSuggestions = result.places;
    activeSuggestion = result.places.length > 0 ? 0 : -1;
    renderSuggestions();
  } catch (error) {
    if (!(error instanceof DOMException && error.name === 'AbortError')) {
      currentSuggestions = [];
      renderSuggestions('No matching place found.');
    }
  }
}

function renderSuggestions(emptyMessage = 'No matching place found.'): void {
  suggestions.hidden = false;
  if (currentSuggestions.length === 0) {
    suggestions.innerHTML = `<li class="place-suggestion-empty">${emptyMessage}</li>`;
    return;
  }
  suggestions.innerHTML = currentSuggestions
    .map((place, index) => `<li id="place-option-${index}" role="option" aria-selected="${index === activeSuggestion}" data-place-index="${index}" class="${index === activeSuggestion ? 'active' : ''}"><strong>${escapeHtml(place.name)}</strong><span>${escapeHtml(place.place_type)} · ${place.lat.toFixed(4)}, ${place.lon.toFixed(4)}</span></li>`)
    .join('');
  placeInput.setAttribute('aria-activedescendant', `place-option-${activeSuggestion}`);
  for (const option of suggestions.querySelectorAll<HTMLElement>('[data-place-index]')) {
    option.addEventListener('mousedown', (event) => {
      event.preventDefault();
      const place = currentSuggestions[Number(option.dataset.placeIndex)];
      if (place) selectPlace(place);
    });
  }
}

function hideSuggestions(): void {
  suggestions.hidden = true;
  currentSuggestions = [];
  activeSuggestion = -1;
  placeInput.removeAttribute('aria-activedescendant');
}

function selectPlace(place: Place): void {
  placeInput.value = place.name;
  hideSuggestions();
  chooseCenter(place, `${place.name} (${place.place_type})`);
}

function chooseCenter(center: Coordinates, label: string): void {
  selectedCenter = center;
  selectedLocationName = label;
  selectedWaterPointKey = null;
  currentResponse = null;
  locationSummary.textContent = `${label} · ${formatCoordinates(center)}`;
  mapEmpty.classList.add('hidden');
  resetViewButton.disabled = false;
  waterCount.textContent = '0 water points';
  waterTable.innerHTML = '<tr><td colspan="4" class="empty-cell">Searching for drinking water...</td></tr>';
  renderSearchGeometry([], true);
  void loadNearby(true);
}

async function loadNearby(fitMap: boolean): Promise<void> {
  if (!selectedCenter) return;
  nearbyController?.abort();
  streetViewController?.abort();
  const controller = new AbortController();
  nearbyController = controller;
  setStatus('Searching local OpenStreetMap data...', 'neutral');
  const parameters = new URLSearchParams({
    lat: String(selectedCenter.lat),
    lon: String(selectedCenter.lon),
    radius_m: String(selectedRadius)
  });
  try {
    const response = await fetch(`/api/water-points/nearby?${parameters}`, { signal: controller.signal });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const result = (await response.json()) as NearbyResponse;
    if (controller.signal.aborted) return;
    currentResponse = result;
    renderNearby(fitMap);
    setStatus(
      result.truncated ? 'Showing the 2,000 nearest water points. More points exist in this radius.' : '',
      result.truncated ? 'neutral' : 'neutral'
    );
    void loadStreetViewLinks(result);
  } catch (error) {
    if (!(error instanceof DOMException && error.name === 'AbortError')) {
      console.error(error);
      setStatus('Nearby water points could not be loaded.', 'error');
    }
  } finally {
    if (nearbyController === controller) nearbyController = null;
  }
}

function renderNearby(fitMap: boolean): void {
  if (!selectedCenter || !currentResponse) return;
  renderSearchGeometry(currentResponse.water_points, fitMap);
  waterCount.textContent = `${currentResponse.water_points.length}${currentResponse.truncated ? '+' : ''} ${currentResponse.water_points.length === 1 ? 'water point' : 'water points'}`;
  renderTable();
}

function renderSearchGeometry(points: NearbyWaterPoint[], fitMap: boolean): void {
  if (!selectedCenter) return;
  markerLayer.clearLayers();
  radiusCircle?.removeFrom(map);
  centerMarker?.removeFrom(map);
  radiusCircle = L.circle([selectedCenter.lat, selectedCenter.lon], {
    radius: selectedRadius,
    color: '#106ba3',
    fillColor: '#4d9fe3',
    fillOpacity: 0.08,
    weight: 2
  }).addTo(map);
  centerMarker = L.circleMarker([selectedCenter.lat, selectedCenter.lon], {
    radius: 8,
    color: '#ffffff',
    fillColor: '#c83d2c',
    fillOpacity: 1,
    weight: 3
  }).bindPopup(escapeHtml(selectedLocationName)).addTo(map);
  for (const point of points) {
    const key = waterPointKey(point);
    const selected = key === selectedWaterPointKey;
    L.marker([point.lat, point.lon], { icon: waterIcon(selected), zIndexOffset: selected ? 1000 : 0 })
      .bindPopup(`<strong>${escapeHtml(point.name ?? 'Water Point')}</strong><br>Distance: ${formatDistance(point.distance_m)}`)
      .on('click', () => selectWaterPoint(key))
      .addTo(markerLayer);
  }
  if (fitMap) fitSearchArea();
}

function renderTable(): void {
  const points = currentResponse?.water_points ?? [];
  if (points.length === 0) {
    waterTable.innerHTML = '<tr><td colspan="4" class="empty-cell">No drinking water found in this radius.</td></tr>';
    return;
  }
  waterTable.innerHTML = points.map((point) => {
    const key = waterPointKey(point);
    const coordinates = formatCoordinates(point);
    return `<tr data-osm-key="${key}" class="${key === selectedWaterPointKey ? 'selected' : ''}" tabindex="0">
      <td><span class="offset-pill">${formatDistance(point.distance_m)}</span></td>
      <td><span>${escapeHtml(point.name ?? 'Water Point')}</span><small>OSM ${point.osm_id}</small></td>
      <td><span class="coordinates-cell"><span class="coordinates">${coordinates}</span><button class="copy-coordinates" type="button" data-coordinates="${coordinates}" title="Copy coordinates" aria-label="Copy coordinates"><svg class="copy-icon" viewBox="0 0 24 24" aria-hidden="true"><rect x="8" y="8" width="11" height="12" rx="2"></rect><path d="M16 8V6a2 2 0 0 0-2-2H7a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h1"></path></svg><svg class="copy-success-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="m5 12 4 4L19 6"></path></svg></button></span></td>
      <td>${streetViewLink(point)}</td>
    </tr>`;
  }).join('');
  for (const row of waterTable.querySelectorAll<HTMLTableRowElement>('tr[data-osm-key]')) {
    row.addEventListener('click', (event) => {
      if (!(event.target instanceof Element) || !event.target.closest('a, button')) selectWaterPoint(row.dataset.osmKey ?? '');
    });
    row.addEventListener('keydown', (event) => {
      if (event.target === row && (event.key === 'Enter' || event.key === ' ')) {
        event.preventDefault();
        selectWaterPoint(row.dataset.osmKey ?? '');
      }
    });
    row.querySelector<HTMLButtonElement>('.copy-coordinates')?.addEventListener('click', (event) => {
      void copyCoordinates(event.currentTarget as HTMLButtonElement);
    });
  }
}

async function loadStreetViewLinks(result: NearbyResponse): Promise<void> {
  if (result.water_points.length === 0) return;
  const controller = new AbortController();
  streetViewController = controller;
  try {
    const response = await fetch('/api/street-view', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, signal: controller.signal,
      body: JSON.stringify({ locations: result.water_points.map((point) => ({ id: waterPointKey(point), lat: point.lat, lon: point.lon })) })
    });
    if (!response.ok) return;
    const streetView = (await response.json()) as StreetViewResponse;
    if (controller.signal.aborted || currentResponse !== result) return;
    const links = new Map(streetView.locations.map((location) => [location.id, location.street_view_url]));
    for (const point of result.water_points) point.street_view_url = links.get(waterPointKey(point));
    renderTable();
  } catch (error) {
    if (!(error instanceof DOMException && error.name === 'AbortError')) console.warn('Street View lookup failed.');
  } finally {
    if (streetViewController === controller) streetViewController = null;
  }
}

function selectWaterPoint(key: string): void {
  selectedWaterPointKey = selectedWaterPointKey === key ? null : key;
  renderNearby(false);
}

function fitSearchArea(): void {
  if (!radiusCircle) return;
  const bounds = radiusCircle.getBounds();
  for (const point of currentResponse?.water_points ?? []) bounds.extend([point.lat, point.lon]);
  map.fitBounds(bounds.pad(0.08), { maxZoom: 16 });
}

function updateRadiusButtons(): void {
  for (const button of radiusButtons) {
    const active = Number(button.dataset.nearbyRadius) === selectedRadius;
    button.classList.toggle('active', active);
    button.setAttribute('aria-pressed', String(active));
  }
  radiusLabel.textContent = selectedRadius < 1_000 ? `${selectedRadius} m` : `${selectedRadius / 1_000} km`;
}

function createTileLayer(style: MapStyle): L.TileLayer {
  const layer = tileLayers[style];
  return L.tileLayer(layer.url, layer.options);
}

function setMapStyle(style: MapStyle): void {
  currentTileLayer.removeFrom(map);
  currentTileLayer = createTileLayer(style).addTo(map);
}

function waterIcon(selected: boolean): L.DivIcon {
  return L.divIcon({ className: selected ? 'water-marker selected' : 'water-marker', html: '<span aria-hidden="true"></span>', iconSize: [30, 42], iconAnchor: [15, 39], popupAnchor: [0, -36] });
}

async function toggleFullscreen(): Promise<void> {
  try {
    if (document.fullscreenElement === mapShell) await document.exitFullscreen();
    else await mapShell.requestFullscreen();
  } catch {
    setStatus('The map could not be opened fullscreen.', 'error');
  }
}

function updateFullscreenButton(): void {
  const fullscreen = document.fullscreenElement === mapShell;
  fullscreenButton.classList.toggle('active', fullscreen);
  fullscreenButton.setAttribute('aria-label', fullscreen ? 'Exit map fullscreen' : 'Open map fullscreen');
  fullscreenButton.title = fullscreen ? 'Exit fullscreen' : 'Fullscreen map';
}

async function copyCoordinates(button: HTMLButtonElement): Promise<void> {
  const coordinates = button.dataset.coordinates;
  if (!coordinates) return;
  try {
    await navigator.clipboard.writeText(coordinates);
    button.classList.add('copied');
    window.setTimeout(() => button.classList.remove('copied'), 1_500);
  } catch {
    console.warn('Coordinates could not be copied.');
  }
}

function streetViewLink(point: NearbyWaterPoint): string {
  if (!point.street_view_url) return '';
  return `<a class="street-view-link" href="${escapeHtml(point.street_view_url)}" target="_blank" rel="noopener noreferrer" title="Open Street View" aria-label="Open Street View near ${escapeHtml(point.name ?? 'water point')}"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 8.5h3.2L9 6h6l1.8 2.5H20v10H4z"></path><circle cx="12" cy="13.5" r="3.5"></circle></svg></a>`;
}

function waterPointKey(point: NearbyWaterPoint): string { return `${point.osm_type}:${point.osm_id}`; }
function formatCoordinates(point: Coordinates): string { return `${point.lat.toFixed(6)}, ${point.lon.toFixed(6)}`; }
function formatDistance(meters: number): string { return meters < 1_000 ? `${Math.round(meters)} m` : `${(meters / 1_000).toFixed(1)} km`; }
function setStatus(message: string, tone: 'neutral' | 'error'): void { statusMessage.textContent = message; statusMessage.dataset.tone = tone; statusLine?.classList.toggle('hidden', message.length === 0); }
function escapeHtml(value: string): string { return value.replace(/[&<>'"]/g, (character) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&quot;' })[character] ?? character); }
function mustQuery<T extends Element>(selector: string): T { const element = document.querySelector<T>(selector); if (!element) throw new Error(`Missing required element: ${selector}`); return element; }

updateRadiusButtons();

export function refreshNearbyMap(): void {
  map.invalidateSize();
}
