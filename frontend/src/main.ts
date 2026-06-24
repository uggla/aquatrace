import '@picocss/pico/css/pico.min.css';
import 'leaflet/dist/leaflet.css';
import L from 'leaflet';
import './style.css';

type RoutePoint = {
  lat: number;
  lon: number;
  ele?: number;
};

type RouteSummary = {
  distance_m: number;
  elevation_gain_m: number;
  elevation_loss_m: number;
  min_elevation_m?: number;
  max_elevation_m?: number;
  points: RoutePoint[];
};

type WaterPoint = {
  osm_id: number;
  name?: string;
  lat: number;
  lon: number;
  km: number;
  distance_to_route_m: number;
};

type AnalyzeResponse = {
  route: RouteSummary;
  water_points: WaterPoint[];
};

const form = mustQuery<HTMLFormElement>('#upload-form');
const fileInput = mustQuery<HTMLInputElement>('#gpx-file');
const analyzeButton = mustQuery<HTMLButtonElement>('#analyze-button');
const statusLine = mustQuery<HTMLElement>('#status-line');
const statusMessage = mustQuery<HTMLParagraphElement>('#status-message');
const distanceMetric = mustQuery<HTMLSpanElement>('#distance-metric');
const gainMetric = mustQuery<HTMLSpanElement>('#gain-metric');
const lossMetric = mustQuery<HTMLSpanElement>('#loss-metric');
const rangeMetric = mustQuery<HTMLSpanElement>('#range-metric');
const distanceLabel = mustQuery<HTMLSpanElement>('#distance-label');
const waterCount = mustQuery<HTMLSpanElement>('#water-count');
const fileSummary = mustQuery<HTMLParagraphElement>('#file-summary');
const mapEmpty = mustQuery<HTMLDivElement>('#map-empty');
const mapShell = mustQuery<HTMLDivElement>('.map-shell');
const fullscreenButton = mustQuery<HTMLButtonElement>('#fullscreen-button');
const waterTable = mustQuery<HTMLTableSectionElement>('#water-table');
const filterButtons = Array.from(document.querySelectorAll<HTMLButtonElement>('[data-distance]'));

let currentAnalysis: AnalyzeResponse | null = null;
let selectedDistance = 200;
let selectedWaterPointId: number | null = null;
let routeLayer: L.Polyline | null = null;
let markerLayer = L.layerGroup();

const map = L.map('map', {
  scrollWheelZoom: true,
  zoomControl: false
}).setView([45.0, 5.0], 9);

L.control.zoom({ position: 'topright' }).addTo(map);

L.tileLayer('https://{s}.tile.opentopomap.org/{z}/{x}/{y}.png', {
  maxZoom: 17,
  attribution: '© OpenStreetMap contributors © OpenTopoMap'
}).addTo(map);

markerLayer.addTo(map);

fullscreenButton.addEventListener('click', () => {
  void toggleFullscreen();
});

document.addEventListener('fullscreenchange', () => {
  updateFullscreenButton();
  window.setTimeout(() => map.invalidateSize(), 100);
});

form.addEventListener('submit', (event) => {
  event.preventDefault();
  void analyzeRoute();
});

for (const button of filterButtons) {
  button.addEventListener('click', () => {
    selectedDistance = Number(button.dataset.distance);
    updateFilterButtons();
    renderAnalysis();
  });
}

async function analyzeRoute(): Promise<void> {
  const file = fileInput.files?.[0];
  if (!file) {
    setStatus('Choose a GPX file first.', 'error');
    return;
  }

  if (!file.name.toLowerCase().endsWith('.gpx')) {
    setStatus('Only GPX files are accepted.', 'error');
    return;
  }

  const formData = new FormData();
  formData.append('file', file);

  setLoading(true);
  fileSummary.textContent = `${file.name} - ${formatFileSize(file.size)}`;
  setStatus('Analyzing route with local OpenStreetMap data...', 'neutral');

  try {
    const response = await fetch('/api/analyze', {
      method: 'POST',
      body: formData
    });

    if (!response.ok) {
      const error = await response.json().catch(() => ({ error: 'request_failed' }));
      throw new Error(error.error ?? `HTTP ${response.status}`);
    }

    currentAnalysis = (await response.json()) as AnalyzeResponse;
    selectedWaterPointId = null;
    renderAnalysis();
    setStatus('', 'neutral');
  } catch (error) {
    console.error(error);
    setStatus(error instanceof Error ? readableError(error.message) : 'The route could not be analyzed.', 'error');
  } finally {
    setLoading(false);
  }
}

function renderAnalysis(): void {
  if (!currentAnalysis) {
    return;
  }

  renderMetrics(currentAnalysis.route);
  renderMap(currentAnalysis);
  const visibleWaterPoints = currentAnalysis.water_points.filter(
    (point) => point.distance_to_route_m <= selectedDistance
  );
  if (
    selectedWaterPointId !== null &&
    !visibleWaterPoints.some((point) => point.osm_id === selectedWaterPointId)
  ) {
    selectedWaterPointId = null;
  }
  waterCount.textContent = `${visibleWaterPoints.length} ${visibleWaterPoints.length === 1 ? 'water point' : 'water points'}`;
  renderTable(visibleWaterPoints);
}

function renderMetrics(route: RouteSummary): void {
  distanceMetric.textContent = `${(route.distance_m / 1000).toFixed(1)} km`;
  gainMetric.textContent = `${Math.round(route.elevation_gain_m)} m`;
  lossMetric.textContent = `${Math.round(route.elevation_loss_m)} m`;
  rangeMetric.textContent =
    route.min_elevation_m !== undefined && route.max_elevation_m !== undefined
      ? `${Math.round(route.min_elevation_m)}-${Math.round(route.max_elevation_m)} m`
      : '-';
}

function renderMap(analysis: AnalyzeResponse): void {
  const coordinates = analysis.route.points.map((point) => L.latLng(point.lat, point.lon));
  const visibleWaterPoints = analysis.water_points.filter(
    (point) => point.distance_to_route_m <= selectedDistance
  );

  if (routeLayer) {
    routeLayer.removeFrom(map);
  }

  markerLayer.clearLayers();
  mapEmpty.classList.add('hidden');
  routeLayer = L.polyline(coordinates, {
    color: '#0b5f88',
    weight: 5,
    opacity: 0.95,
    lineCap: 'round',
    lineJoin: 'round'
  }).addTo(map);

  for (const point of visibleWaterPoints) {
    const selected = point.osm_id === selectedWaterPointId;
    L.marker([point.lat, point.lon], { icon: waterIcon(selected), zIndexOffset: selected ? 1000 : 0 })
      .bindPopup(
        `<strong>${escapeHtml(point.name ?? 'Drinking Water')}</strong><br>Km: ${point.km.toFixed(
          1
        )}<br>Distance: ${Math.round(point.distance_to_route_m)} m`
      )
      .addTo(markerLayer);
  }

  const bounds = routeLayer.getBounds();
  for (const point of visibleWaterPoints) {
    bounds.extend([point.lat, point.lon]);
  }
  map.fitBounds(bounds.pad(0.12), { maxZoom: 15 });
}

function renderTable(points: WaterPoint[]): void {
  if (points.length === 0) {
    waterTable.innerHTML =
      '<tr><td colspan="3" class="empty-cell">No drinking water found in this distance range.</td></tr>';
    return;
  }

  waterTable.innerHTML = points
    .map(
      (point) => `<tr data-osm-id="${point.osm_id}" class="${point.osm_id === selectedWaterPointId ? 'selected' : ''}" tabindex="0">
        <td><strong>${point.km.toFixed(1)}</strong></td>
        <td><span class="offset-pill">${Math.round(point.distance_to_route_m)} m</span></td>
        <td>
          <span>${escapeHtml(point.name ?? 'Water Point')}</span>
          <small>OSM ${point.osm_id}</small>
        </td>
      </tr>`
    )
    .join('');

  for (const row of waterTable.querySelectorAll<HTMLTableRowElement>('tr[data-osm-id]')) {
    row.addEventListener('click', () => selectWaterPoint(Number(row.dataset.osmId)));
    row.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        selectWaterPoint(Number(row.dataset.osmId));
      }
    });
  }
}

function updateFilterButtons(): void {
  for (const button of filterButtons) {
    const active = Number(button.dataset.distance) === selectedDistance;
    button.classList.toggle('active', active);
    button.setAttribute('aria-pressed', String(active));
  }
  distanceLabel.textContent = `${selectedDistance} m`;
}

function setLoading(loading: boolean): void {
  analyzeButton.disabled = loading;
  analyzeButton.textContent = loading ? 'Analyzing...' : 'Analyze route';
  document.body.classList.toggle('is-loading', loading);
}

function setStatus(message: string, tone: 'neutral' | 'success' | 'error'): void {
  statusMessage.textContent = message;
  statusMessage.dataset.tone = tone;
  statusLine.classList.toggle('hidden', message.length === 0);
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (char) => {
    const escapes: Record<string, string> = {
      '&': '&amp;',
      '<': '&lt;',
      '>': '&gt;',
      '"': '&quot;',
      "'": '&#039;'
    };
    return escapes[char];
  });
}

function selectWaterPoint(osmId: number): void {
  selectedWaterPointId = selectedWaterPointId === osmId ? null : osmId;
  renderAnalysis();
}

function waterIcon(selected: boolean): L.DivIcon {
  return L.divIcon({
    className: selected ? 'water-marker selected' : 'water-marker',
    html: '<span aria-hidden="true"></span>',
    iconSize: [30, 42],
    iconAnchor: [15, 39],
    popupAnchor: [0, -36]
  });
}

async function toggleFullscreen(): Promise<void> {
  if (!document.fullscreenEnabled) {
    setStatus('Fullscreen mode is not available in this browser.', 'error');
    return;
  }

  try {
    if (document.fullscreenElement === mapShell) {
      await document.exitFullscreen();
      return;
    }

    await mapShell.requestFullscreen();
  } catch (error) {
    console.error(error);
    setStatus('The map could not be opened fullscreen.', 'error');
  }
}

function updateFullscreenButton(): void {
  const fullscreen = document.fullscreenElement === mapShell;
  fullscreenButton.classList.toggle('active', fullscreen);
  fullscreenButton.setAttribute('aria-label', fullscreen ? 'Exit map fullscreen' : 'Open map fullscreen');
  fullscreenButton.title = fullscreen ? 'Exit fullscreen' : 'Fullscreen map';
}

function readableError(code: string): string {
  const messages: Record<string, string> = {
    bad_request: 'The GPX file is invalid or does not contain a usable track.',
    payload_too_large: 'The GPX file is too large. Maximum size is 10 MB.',
    internal_server_error: 'The route could not be analyzed. Check the backend logs for details.',
    request_failed: 'The backend did not return a readable response.'
  };

  return messages[code] ?? 'The route could not be analyzed.';
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024 * 1024) {
    return `${Math.round(bytes / 1024)} KB`;
  }

  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function mustQuery<T extends Element>(selector: string): T {
  const element = document.querySelector<T>(selector);
  if (!element) {
    throw new Error(`Missing required element: ${selector}`);
  }
  return element;
}

updateFilterButtons();
