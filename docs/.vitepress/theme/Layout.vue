<script setup>
import DefaultTheme from 'vitepress/theme'
import { useData, withBase } from 'vitepress'
const { page } = useData()
</script>

<template>
  <DefaultTheme.Layout>
    <template #doc-before>
      <div class="source-link">
        <a :href="withBase('/' + page.relativePath)">View Markdown for agents ↗</a>
      </div>
    </template>
  </DefaultTheme.Layout>
</template>
