use std::borrow::Borrow;
use std::sync::RwLock;

use lazy_static::lazy_static;
use rusqlite::{Connection, Row, Rows, ToSql};
use ublog_models::posts::{Post, PostResource};

use crate::masks::PostUpdateMask;
use crate::models::Model;
use crate::Pagination;

impl Model for Post {
    type SelectKey = str;
    type UpdateMask = PostUpdateMask;

    fn init_db_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        const INIT_SQL: &str = r#"
            CREATE TABLE IF NOT EXISTS posts (
                id               INTEGER PRIMARY KEY,
                title            TEXT NOT NULL,
                slug             TEXT NOT NULL,
                author           TEXT NOT NULL,
                create_timestamp INTEGER NOT NULL,
                update_timestamp INTEGER NOT NULL,
                category         TEXT NOT NULL,
                views            INTEGER NOT NULL,
                content          TEXT NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS posts_idx_slug     ON posts (slug);
            CREATE INDEX IF NOT EXISTS        posts_idx_ts       ON posts (create_timestamp DESC);
            CREATE INDEX IF NOT EXISTS        posts_idx_category ON posts (category);
            CREATE INDEX IF NOT EXISTS        posts_idx_views    ON posts (views DESC);

            CREATE TABLE IF NOT EXISTS posts_tags (
                post_id  TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                tag_name TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS        posts_tags_idx_tag_name ON posts_tags (tag_name);
            CREATE UNIQUE INDEX IF NOT EXISTS posts_tags_idx_uniq     ON posts_tags (post_id, tag_name);
        "#;

        conn.execute_batch(INIT_SQL)
    }

    fn select_one_from<K>(conn: &RwLock<Connection>, key: &K) -> Result<Self, rusqlite::Error>
    where
        K: ?Sized + Borrow<Self::SelectKey>,
    {
        const SELECT_SQL: &str = r#"
            SELECT id, title, slug, author, create_timestamp, update_timestamp, category, views, content
            FROM posts
            WHERE slug == ?;
        "#;

        let conn = conn.read().unwrap();

        let slug: &str = key.borrow();
        let mut post = conn.query_row(SELECT_SQL, (slug,), Self::from_row)?;

        select_tags_for_post(&*conn, &mut post)?;

        Ok(post)
    }

    fn select_many_from(
        conn: &RwLock<Connection>,
        pagination: &Pagination,
    ) -> Result<Vec<Self>, rusqlite::Error> {
        const SELECT_SQL: &str = r#"
            SELECT id, title, slug, author, create_timestamp, update_timestamp, category, views, content
            FROM posts
            ORDER BY create_timestamp DESC
            LIMIT ? OFFSET ?;
        "#;

        let conn = conn.read().unwrap();

        let limit = pagination.page_size;
        let offset = pagination.skip_count();

        let mut query_stmt = conn.prepare_cached(SELECT_SQL).unwrap();
        let post_rows = query_stmt.query((limit, offset))?;
        Self::from_rows(post_rows)
    }

    fn insert_into(&mut self, conn: &RwLock<Connection>) -> Result<(), rusqlite::Error> {
        const INSERT_POST_SQL: &str = r#"
            INSERT INTO posts (title, slug, author, create_timestamp, update_timestamp, category, views, content)
            VALUES (?, ?, ?, ?, ?, ?, 0, ?);
        "#;

        let mut conn = conn.write().unwrap();
        let trans = conn.transaction()?;

        let create_timestamp = now_utc_unix_timestamp();
        let update_timestamp = create_timestamp;

        // Insert the post object into the database.
        trans.execute(
            INSERT_POST_SQL,
            (
                &self.title,
                &self.slug,
                &self.author,
                create_timestamp,
                update_timestamp,
                &self.category,
                &self.content,
            ),
        )?;
        self.id = trans.last_insert_rowid();
        self.create_timestamp = create_timestamp;
        self.update_timestamp = update_timestamp;

        // Insert tags into the database.
        if !self.tags.is_empty() {
            insert_post_tags(&trans, self.id, &self.tags)?;
        }

        trans.commit()?;

        Ok(())
    }

    fn update_into(
        &mut self,
        conn: &RwLock<Connection>,
        mask: &Self::UpdateMask,
    ) -> Result<(), rusqlite::Error> {
        if mask.is_empty() {
            return Ok(());
        }

        let update_timestamp = now_utc_unix_timestamp();

        let mut column_names: Vec<&'static str> = vec!["update_timestamp"];
        let mut column_parameters: Vec<&'static str> = vec!["?"];
        let mut column_values: Vec<&dyn ToSql> = vec![&update_timestamp];

        for field in &*POST_FIELDS {
            if mask.contains(field.mask) {
                column_names.push(field.name);
                column_parameters.push("?");
                column_values.push((field.field_getter)(self));
            }
        }

        let update_post_sql = format!(
            r#"
            UPDATE posts ({})
            VALUES ({})
        "#,
            column_names.join(","),
            column_parameters.join(",")
        );

        let mut conn = conn.write().unwrap();
        let trans = conn.transaction()?;

        // Update the post object itself.
        trans.execute(&update_post_sql, column_values.as_slice())?;

        // Update the post's tags, if any.
        if mask.contains(PostUpdateMask::TAGS) {
            // Delete all old tags.
            const DELETE_TAGS_SQL: &str = r#"
                DELETE FROM posts_tags
                WHERE post_id == ?;
            "#;
            trans.execute(DELETE_TAGS_SQL, (self.id,))?;

            // Insert all new tags.
            insert_post_tags(&trans, self.id, &self.tags)?;
        }

        trans.commit()?;

        self.update_timestamp = update_timestamp;
        Ok(())
    }

    fn delete_from<K>(conn: &RwLock<Connection>, key: &K) -> Result<(), rusqlite::Error>
    where
        K: ?Sized + Borrow<Self::SelectKey>,
    {
        const DELETE_SQL: &str = r#"
            DELETE FROM posts
            WHERE slug == ?;
        "#;

        let conn = conn.read().unwrap();

        let slug: &str = key.borrow();
        conn.execute(DELETE_SQL, (slug,))?;

        Ok(())
    }

    fn from_row(row: &Row) -> Result<Self, rusqlite::Error> {
        Ok(Post {
            id: row.get("id")?,
            title: row.get("title")?,
            slug: row.get("slug")?,
            author: row.get("author")?,
            create_timestamp: row.get("create_timestamp")?,
            update_timestamp: row.get("update_timestamp")?,
            category: row.get("category")?,
            tags: Vec::new(),
            views: row.get("views")?,
            content: row.get("content")?,
        })
    }
}

pub(crate) trait PostModelExt {
    fn increase_views(&mut self, conn: &RwLock<Connection>) -> Result<(), rusqlite::Error>;
}

impl PostModelExt for Post {
    fn increase_views(&mut self, conn: &RwLock<Connection>) -> Result<(), rusqlite::Error> {
        let mut conn = conn.write().unwrap();

        let trans = conn.transaction()?;

        // Update the latest views count.
        const SELECT_VIEWS_SQL: &str = r#"
            SELECT views FROM posts
            WHERE id == ?;
        "#;
        let old_views: u64 = trans.query_row(SELECT_VIEWS_SQL, (self.id,), |row| row.get(0))?;
        self.views = old_views + 1;

        const UPDATE_VIEWS_SQL: &str = r#"
            UPDATE posts
            SET views = ?
            WHERE id == ?;
        "#;
        trans.execute(UPDATE_VIEWS_SQL, (self.views, self.id))?;

        trans.commit()?;
        Ok(())
    }
}

impl Model for PostResource {
    type SelectKey = (i64, String);
    type UpdateMask = ();

    fn init_db_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        const INIT_SQL: &str = r#"
            CREATE TABLE IF NOT EXISTS posts_resources (
                post_id  INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
                res_name TEXT NOT NULL,
                res_type TEXT NOT NULL,
                res_data BLOB NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS posts_resources_idx_name_uniq ON posts_resources (post_id, res_name);
        "#;
        conn.execute_batch(INIT_SQL)
    }

    fn select_one_from<K>(conn: &RwLock<Connection>, key: &K) -> Result<Self, rusqlite::Error>
    where
        K: ?Sized + Borrow<Self::SelectKey>,
    {
        const SELECT_SQL: &str = r#"
            SELECT post_id, res_name, res_type, res_data FROM posts_resources
            WHERE post_id == ? AND res_name == ?;
        "#;

        let (post_id, res_name) = key.borrow();

        let conn = conn.read().unwrap();
        conn.query_row(SELECT_SQL, (post_id, res_name), Self::from_row)
    }

    fn select_many_from(
        _conn: &RwLock<Connection>,
        _pagination: &Pagination,
    ) -> Result<Vec<Self>, rusqlite::Error> {
        panic!("Selecting a list of post resource objects is not a supported operation.");
    }

    fn insert_into(&mut self, conn: &RwLock<Connection>) -> Result<(), rusqlite::Error> {
        const INSERT_SQL: &str = r#"
            INSERT INTO posts_resources (post_id, res_name, res_type, res_data)
            VALUES (?, ?, ?, ?);
        "#;

        let conn = conn.read().unwrap();
        conn.execute(INSERT_SQL, (self.post_id, &self.name, &self.ty, &self.data))?;
        Ok(())
    }

    fn update_into(
        &mut self,
        _conn: &RwLock<Connection>,
        _mask: &Self::UpdateMask,
    ) -> Result<(), rusqlite::Error> {
        panic!("Updating post resource object is not a supported operation.");
    }

    fn delete_from<K>(conn: &RwLock<Connection>, key: &K) -> Result<(), rusqlite::Error>
    where
        K: ?Sized + Borrow<Self::SelectKey>,
    {
        const DELETE_SQL: &str = r#"
            DELETE FROM posts_resources
            WHERE post_id == ? AND res_name == ?;
        "#;

        let conn = conn.read().unwrap();

        let (post_id, res_name) = key.borrow();
        conn.execute(DELETE_SQL, (post_id, res_name))?;
        Ok(())
    }

    fn from_row(row: &Row) -> Result<Self, rusqlite::Error> {
        Ok(Self {
            post_id: row.get("post_id")?,
            name: row.get("res_name")?,
            ty: row.get("res_ty")?,
            data: row.get("res_data")?,
        })
    }

    fn from_rows(mut rows: Rows) -> Result<Vec<Self>, rusqlite::Error> {
        let mut ret = Vec::new();

        while let Some(row) = rows.next()? {
            ret.push(Self {
                post_id: row.get("post_id")?,
                name: row.get("res_name")?,
                ty: row.get("res_ty")?,
                data: Vec::new(),
            });
        }

        Ok(ret)
    }
}

fn now_utc_unix_timestamp() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn select_tags_for_post(conn: &Connection, post: &mut Post) -> Result<(), rusqlite::Error> {
    const SELECT_SQL: &str = r#"
        SELECT tag_name FROM posts_tags
        WHERE post_id == ?;
    "#;

    let mut select_stmt = conn.prepare_cached(SELECT_SQL).unwrap();
    let rows = select_stmt.query((post.id,))?;

    post.tags = rows
        .mapped(|row| row.get(0))
        .collect::<Result<Vec<String>, rusqlite::Error>>()?;

    Ok(())
}

fn insert_post_tags(
    conn: &Connection,
    post_id: i64,
    tags: &[String],
) -> Result<(), rusqlite::Error> {
    let column_parameters: Vec<&'static str> = vec!["(?1, ?)"; tags.len()];

    let mut param_values: Vec<&dyn ToSql> = vec![&post_id];
    param_values.reserve(tags.len());
    for t in tags {
        param_values.push(t);
    }

    let insert_tags_sql = format!(
        r#"
        INSERT INTO posts_tags (post_id, tag_name)
        VALUES {}
    "#,
        column_parameters.join(",")
    );

    conn.execute(&insert_tags_sql, param_values.as_slice())?;

    Ok(())
}

struct PostFieldDescriptor {
    mask: PostUpdateMask,
    name: &'static str,
    field_getter: Box<dyn Send + Sync + Fn(&Post) -> &dyn ToSql>,
}

impl PostFieldDescriptor {
    fn new<G>(mask: PostUpdateMask, name: &'static str, getter: G) -> Self
    where
        G: 'static + Send + Sync + Fn(&Post) -> &dyn ToSql,
    {
        Self {
            mask,
            name,
            field_getter: Box::new(getter),
        }
    }
}

macro_rules! make_post_field_descriptor {
    ( $mask:expr, $field_name:ident ) => {
        PostFieldDescriptor::new($mask, stringify!($field_name), |post| &post.$field_name)
    };
}

lazy_static! {
    static ref POST_FIELDS: Vec<PostFieldDescriptor> = vec![
        make_post_field_descriptor!(PostUpdateMask::TITLE, title),
        make_post_field_descriptor!(PostUpdateMask::SLUG, slug),
        make_post_field_descriptor!(PostUpdateMask::AUTHOR, author),
        make_post_field_descriptor!(PostUpdateMask::CATEGORY, category),
        make_post_field_descriptor!(PostUpdateMask::CONTENT, content),
    ];
}
