// SPDX-License-Identifier: GPL-3.0-or-later
//
// "Send with ldrop" directly in the Nautilus (GNOME Files) context menu: a native Nautilus
// extension, so it needs no nautilus-python. Built by `make` when libnautilus-extension-4 is
// installed, and installed into Nautilus' own extension directory; Nautilus loads it on its
// next start (nautilus -q).

#include <gio/gio.h>
#include <nautilus-extension.h>

#ifndef LDROP_GUI
#define LDROP_GUI "ldrop-gui"
#endif

#define LDROP_TYPE_MENU (ldrop_menu_get_type ())
G_DECLARE_FINAL_TYPE (LdropMenu, ldrop_menu, LDROP, MENU, GObject)

struct _LdropMenu
{
    GObject parent_instance;
};

static void menu_provider_init (NautilusMenuProviderInterface *iface);

G_DEFINE_DYNAMIC_TYPE_EXTENDED (LdropMenu, ldrop_menu, G_TYPE_OBJECT, 0,
                                G_IMPLEMENT_INTERFACE_DYNAMIC (NAUTILUS_TYPE_MENU_PROVIDER,
                                                               menu_provider_init))

static void
send_activated (NautilusMenuItem *item, gpointer user_data)
{
    GStrv paths = g_object_get_data (G_OBJECT (item), "ldrop-paths");
    g_autoptr (GStrvBuilder) builder = g_strv_builder_new ();
    g_strv_builder_add (builder, LDROP_GUI);
    g_strv_builder_add (builder, "--share");
    g_strv_builder_addv (builder, (const char **) paths);
    g_auto (GStrv) argv = g_strv_builder_end (builder);

    g_autoptr (GError) error = NULL;
    if (!g_spawn_async (NULL, argv, NULL, G_SPAWN_SEARCH_PATH, NULL, NULL, NULL, &error))
        g_warning ("ldrop: cannot start %s: %s", LDROP_GUI, error->message);
}

// Regular local files only, like the other file manager entries.
static GList *
get_file_items (NautilusMenuProvider *provider, GList *files)
{
    if (files == NULL)
        return NULL;

    g_autoptr (GStrvBuilder) builder = g_strv_builder_new ();
    for (GList *l = files; l != NULL; l = l->next) {
        NautilusFileInfo *file = l->data;
        if (nautilus_file_info_is_directory (file))
            return NULL;
        g_autoptr (GFile) location = nautilus_file_info_get_location (file);
        g_autofree char *path = g_file_get_path (location);
        if (path == NULL)
            return NULL;
        g_strv_builder_add (builder, path);
    }

    NautilusMenuItem *item = nautilus_menu_item_new ("LdropMenu::send", "Send with ldrop",
                                                     "Send to a nearby phone or computer",
                                                     "document-share");
    g_object_set_data_full (G_OBJECT (item), "ldrop-paths", g_strv_builder_end (builder),
                            (GDestroyNotify) g_strfreev);
    g_signal_connect (item, "activate", G_CALLBACK (send_activated), NULL);
    return g_list_append (NULL, item);
}

static void
menu_provider_init (NautilusMenuProviderInterface *iface)
{
    iface->get_file_items = get_file_items;
}

static void
ldrop_menu_init (LdropMenu *self)
{
}

static void
ldrop_menu_class_init (LdropMenuClass *klass)
{
}

static void
ldrop_menu_class_finalize (LdropMenuClass *klass)
{
}

static GType types[1];

void
nautilus_module_initialize (GTypeModule *module)
{
    ldrop_menu_register_type (module);
    types[0] = LDROP_TYPE_MENU;
}

void
nautilus_module_shutdown (void)
{
}

void
nautilus_module_list_types (const GType **out_types, int *num_types)
{
    *out_types = types;
    *num_types = G_N_ELEMENTS (types);
}
