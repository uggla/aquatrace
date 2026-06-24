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
const elevationProfile = mustQuery<HTMLDivElement>('#elevation-profile');
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
  renderElevationProfile(currentAnalysis.route, visibleWaterPoints);
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
      .on('click', () => selectWaterPoint(point.osm_id))
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

function renderElevationProfile(route: RouteSummary, waterPoints: WaterPoint[]): void {
  const series = elevationSeries(route);
  if (series.length < 2) {
    elevationProfile.innerHTML = '<p>No elevation data available for this route.</p>';
    return;
  }

  const width = 920;
  const height = 260;
  const margin = { top: 16, right: 18, bottom: 34, left: 54 };
  const plotWidth = width - margin.left - margin.right;
  const plotHeight = height - margin.top - margin.bottom;
  const maxDistance = Math.max(series.at(-1)?.distanceM ?? route.distance_m, route.distance_m, 1);
  const elevations = series.map((point) => point.elevationM);
  const minElevation = Math.min(...elevations);
  const maxElevation = Math.max(...elevations);
  const elevationPadding = Math.max((maxElevation - minElevation) * 0.08, 12);
  const yMin = minElevation - elevationPadding;
  const yMax = maxElevation + elevationPadding;

  const xForDistance = (distanceM: number) =>
    margin.left + (Math.min(Math.max(distanceM, 0), maxDistance) / maxDistance) * plotWidth;
  const yForElevation = (elevationM: number) =>
    margin.top + ((yMax - elevationM) / Math.max(yMax - yMin, 1)) * plotHeight;
  const profilePoints = downsampleSeries(series, 900)
    .map((point) => `${xForDistance(point.distanceM).toFixed(1)},${yForElevation(point.elevationM).toFixed(1)}`)
    .join(' ');
  const xTicks = buildTicks(0, maxDistance / 1000, 5);
  const yTicks = buildTicks(yMin, yMax, 4);

  elevationProfile.innerHTML = `
    <svg viewBox="0 0 ${width} ${height}" role="img" aria-label="Elevation profile with water points">
      <rect class="profile-plot" x="${margin.left}" y="${margin.top}" width="${plotWidth}" height="${plotHeight}"></rect>
      ${yTicks
        .map((tick) => {
          const y = yForElevation(tick);
          return `<line class="profile-grid" x1="${margin.left}" y1="${y.toFixed(1)}" x2="${
            width - margin.right
          }" y2="${y.toFixed(1)}"></line>
          <text class="profile-axis-label" x="${margin.left - 10}" y="${(y + 4).toFixed(1)}" text-anchor="end">${Math.round(
            tick
          )} m</text>`;
        })
        .join('')}
      ${xTicks
        .map((tick) => {
          const x = xForDistance(tick * 1000);
          return `<line class="profile-tick" x1="${x.toFixed(1)}" y1="${height - margin.bottom}" x2="${x.toFixed(
            1
          )}" y2="${height - margin.bottom + 5}"></line>
          <text class="profile-axis-label" x="${x.toFixed(1)}" y="${height - 10}" text-anchor="middle">${formatTick(
            tick
          )} km</text>`;
        })
        .join('')}
      <polyline class="profile-line" points="${profilePoints}"></polyline>
      ${waterPoints
        .map((point) => {
          const distanceM = point.km * 1000;
          const elevation = interpolateElevation(series, distanceM);
          if (elevation === null) {
            return '';
          }
          const selected = point.osm_id === selectedWaterPointId;
          return `<g class="profile-marker-hit" data-osm-id="${point.osm_id}" role="button" tabindex="0" aria-label="${escapeHtml(
            point.name ?? 'Water point'
          )} at ${point.km.toFixed(1)} km">
            <circle class="profile-marker ${selected ? 'selected' : ''}" cx="${xForDistance(distanceM).toFixed(
              1
            )}" cy="${yForElevation(elevation).toFixed(1)}" r="${selected ? 7 : 6}"></circle>
          </g>`;
        })
        .join('')}
    </svg>`;

  for (const marker of elevationProfile.querySelectorAll<SVGElement>('.profile-marker-hit')) {
    marker.addEventListener('click', () => selectWaterPoint(Number(marker.dataset.osmId)));
    marker.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.key === ' ') {
        event.preventDefault();
        selectWaterPoint(Number(marker.dataset.osmId));
      }
    });
  }
}

function elevationSeries(route: RouteSummary): Array<{ distanceM: number; elevationM: number }> {
  const series: Array<{ distanceM: number; elevationM: number }> = [];
  let distanceM = 0;

  for (const [index, point] of route.points.entries()) {
    if (index > 0) {
      const previous = route.points[index - 1];
      distanceM += distanceBetween(previous, point);
    }
    if (point.ele !== undefined && Number.isFinite(point.ele)) {
      series.push({ distanceM, elevationM: point.ele });
    }
  }

  return series;
}

function downsampleSeries<T>(series: T[], maxPoints: number): T[] {
  if (series.length <= maxPoints) {
    return series;
  }

  const sampled: T[] = [];
  const step = (series.length - 1) / (maxPoints - 1);
  for (let index = 0; index < maxPoints; index += 1) {
    sampled.push(series[Math.round(index * step)]);
  }
  return sampled;
}

function interpolateElevation(series: Array<{ distanceM: number; elevationM: number }>, distanceM: number): number | null {
  if (series.length === 0) {
    return null;
  }
  if (distanceM <= series[0].distanceM) {
    return series[0].elevationM;
  }

  for (let index = 1; index < series.length; index += 1) {
    const current = series[index];
    if (distanceM <= current.distanceM) {
      const previous = series[index - 1];
      const span = Math.max(current.distanceM - previous.distanceM, 1);
      const ratio = (distanceM - previous.distanceM) / span;
      return previous.elevationM + (current.elevationM - previous.elevationM) * ratio;
    }
  }

  return series.at(-1)?.elevationM ?? null;
}

function buildTicks(min: number, max: number, count: number): number[] {
  if (max <= min) {
    return [min];
  }
  const ticks: number[] = [];
  const step = (max - min) / Math.max(count - 1, 1);
  for (let index = 0; index < count; index += 1) {
    ticks.push(min + step * index);
  }
  return ticks;
}

function formatTick(value: number): string {
  return value >= 10 ? String(Math.round(value)) : value.toFixed(1);
}

function distanceBetween(a: RoutePoint, b: RoutePoint): number {
  const earthRadiusM = 6_371_000;
  const lat1 = toRadians(a.lat);
  const lat2 = toRadians(b.lat);
  const deltaLat = toRadians(b.lat - a.lat);
  const deltaLon = toRadians(b.lon - a.lon);
  const haversine =
    Math.sin(deltaLat / 2) * Math.sin(deltaLat / 2) +
    Math.cos(lat1) * Math.cos(lat2) * Math.sin(deltaLon / 2) * Math.sin(deltaLon / 2);
  return earthRadiusM * 2 * Math.atan2(Math.sqrt(haversine), Math.sqrt(1 - haversine));
}

function toRadians(value: number): number {
  return (value * Math.PI) / 180;
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
