// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

import { createRouter, createWebHistory } from 'vue-router'
import RepositoryList from './views/RepositoryList.vue'
import RepositoryDetail from './views/RepositoryDetail.vue'
import CreateRepository from './views/CreateRepository.vue'
import StoreList from './views/StoreList.vue'
import StoreDetail from './views/StoreDetail.vue'
import CreateStore from './views/CreateStore.vue'
import UserList from './views/UserList.vue'
import UserDetail from './views/UserDetail.vue'
import RoleList from './views/RoleList.vue'
import RoleDetail from './views/RoleDetail.vue'
import SettingsView from './views/Settings.vue'
import Tasks from './views/Tasks.vue'
import Backup from './views/Backup.vue'
import Activity from './views/Activity.vue'
import ApiDocs from './views/ApiDocs.vue'
import Login from './views/Login.vue'
import NotFound from './views/NotFound.vue'
import { getToken } from './api'

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', redirect: '/repositories' },
    { path: '/login', name: 'login', component: Login, meta: { public: true } },
    { path: '/repositories', name: 'repositories', component: RepositoryList },
    { path: '/repositories/new', name: 'create-repository', component: CreateRepository },
    { path: '/repositories/:name', name: 'repository-detail', component: RepositoryDetail },
    { path: '/stores', name: 'stores', component: StoreList },
    { path: '/stores/new', name: 'create-store', component: CreateStore },
    { path: '/stores/:name', name: 'store-detail', component: StoreDetail },
    { path: '/users', name: 'users', component: UserList },
    { path: '/users/new', name: 'user-create', component: UserDetail },
    { path: '/users/:username', name: 'user-detail', component: UserDetail },
    { path: '/roles', name: 'roles', component: RoleList },
    { path: '/roles/new', name: 'role-create', component: RoleDetail },
    { path: '/roles/:name', name: 'role-detail', component: RoleDetail },
    { path: '/settings', name: 'settings', component: SettingsView },
    { path: '/tasks', name: 'tasks', component: Tasks },
    { path: '/backup', name: 'backup', component: Backup },
    { path: '/activity', name: 'activity', component: Activity },
    { path: '/api-docs', name: 'api-docs', component: ApiDocs },
    { path: '/:pathMatch(.*)*', name: 'not-found', component: NotFound },
  ],
})

router.beforeEach((to) => {
  if (!to.meta?.public && !getToken()) {
    return '/login'
  }
})
