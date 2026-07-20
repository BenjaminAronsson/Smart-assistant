import { Routes } from '@angular/router';
import { Conversation } from './conversation';

export const routes: Routes = [
  {
    path: 'sessions/:id',
    component: Conversation,
  },
];
