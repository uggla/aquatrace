import '@picocss/pico/css/pico.min.css';
import 'leaflet/dist/leaflet.css';
import './style.css';
import { refreshRouteMap } from './route-analysis';
import { refreshNearbyMap } from './nearby-search';

type ViewName = 'route' | 'nearby';

const navigationButtons = Array.from(document.querySelectorAll<HTMLAnchorElement>('[data-view]'));
const viewPanels = Array.from(document.querySelectorAll<HTMLElement>('[data-view-panel]'));

function activeView(): ViewName {
  return window.location.hash === '#nearby' ? 'nearby' : 'route';
}

function renderActiveView(): void {
  const view = activeView();
  for (const button of navigationButtons) {
    const active = button.dataset.view === view;
    button.classList.toggle('active', active);
    button.setAttribute('aria-selected', String(active));
    button.tabIndex = active ? 0 : -1;
  }
  for (const panel of viewPanels) {
    panel.hidden = panel.dataset.viewPanel !== view;
  }
  window.setTimeout(() => {
    if (view === 'nearby') {
      refreshNearbyMap();
    } else {
      refreshRouteMap();
    }
  }, 0);
}

window.addEventListener('hashchange', renderActiveView);
renderActiveView();
