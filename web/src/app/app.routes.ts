import { Routes } from '@angular/router';
import { Conversation } from './conversation';

/**
 * `data.surface` says which layer a route belongs to (docs/12 §1): `hud` routes
 * render on the front face, everything else inside the operator layer. The
 * shell reads it — a route with no `surface` is treated as an ops surface,
 * which is the conservative default for anything transcript-shaped.
 */
export const routes: Routes = [
  {
    path: 'sessions/:id',
    component: Conversation,
    data: { surface: 'ops' },
  },
  {
    // ArtifactCanvas (F3b.3, docs/02 §6): its own lazy chunk per the
    // angular-shell skill's bundle discipline — nothing here loads unless an
    // artifact route is actually visited.
    path: 'artifacts/:id',
    loadComponent: () => import('./artifacts/artifact-canvas').then((m) => m.ArtifactCanvas),
    data: { surface: 'hud' },
  },
];
