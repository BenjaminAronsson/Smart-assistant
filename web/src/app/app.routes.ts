import { Routes } from '@angular/router';
import { Conversation } from './conversation';

export const routes: Routes = [
  {
    path: 'sessions/:id',
    component: Conversation,
  },
  {
    // ArtifactCanvas (F3b.3, docs/02 §6): its own lazy chunk per the
    // angular-shell skill's bundle discipline — nothing here loads unless an
    // artifact route is actually visited.
    path: 'artifacts/:id',
    loadComponent: () => import('./artifacts/artifact-canvas').then((m) => m.ArtifactCanvas),
  },
];
